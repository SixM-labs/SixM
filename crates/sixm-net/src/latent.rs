//! Latent events: events too large for a frame, sent on a unidirectional stream
//! of their own so that they never delay the events on the main stream.
//!
//! After its kind byte ([`STREAM_KIND_LATENT`]) a latent stream carries the
//! sequence number of the event as a `u64` and the length of its body as a
//! `u32`, both little-endian, then the body (see
//! [`encode_latent_body`](crate::wire)). The sender numbers the events of one
//! connection from zero and sends them one at a time. QUIC does not keep the
//! order of streams, so the receiver releases the events in sequence order
//! through a [`Sequencer`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use quinn::{Connection, RecvStream, VarInt};
use sixm_core::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::debug;

use crate::error::NetError;
use crate::protocol::{
    DEFAULT_LATENT_RATE, LATENT_ABORTED, LATENT_GRACE, LATENT_REFUSED, MAX_LATENT_EVENT_LEN,
    MAX_LATENT_RATE, MAX_LATENT_STREAMS, MIN_LATENT_RATE, STREAM_KIND_LATENT,
};
use crate::traffic::Counters;
use crate::wire::decode_latent_body;

pub(crate) fn encode_body(name: &str, args: &[Value]) -> Result<Vec<u8>, NetError> {
    crate::wire::encode_latent_body(name, args)
}

/// Bytes after the kind byte: the sequence number and the body length.
pub(crate) const HEADER_LEN: usize = 8 + 4;
/// Smallest number of bytes written at once. Slower rates write this much.
const MIN_CHUNK_LEN: u64 = 1024;
/// Largest number of bytes written at once.
const MAX_CHUNK_LEN: u64 = 64 * 1024;
/// Chunks per second at the pacing rate. A chunk is a twentieth of a second's
/// worth of bytes, so pacing stays smooth at low rates.
const CHUNKS_PER_SECOND: u64 = 20;
/// A stream must make progress at least this often, even when its total budget is long.
const LATENT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maps zero to the default rate and clamps other requests to the supported range.
pub(crate) fn pacing_rate(bytes_per_second: u64) -> u64 {
    if bytes_per_second == 0 {
        DEFAULT_LATENT_RATE
    } else {
        bytes_per_second.clamp(MIN_LATENT_RATE, MAX_LATENT_RATE)
    }
}

/// A latent event waiting to be sent: its encoded body, and the rate at which
/// the body is paced, already clamped by [`pacing_rate`].
#[derive(Debug, Clone)]
pub(crate) struct LatentJob {
    pub(crate) body: Arc<Vec<u8>>,
    pub(crate) bytes_per_second: u64,
}

/// Sends `job` on a new stream as the next event of this direction, numbered
/// `next_seq`. The body is written in chunks, and the writer sleeps between
/// them so that the average rate is the job's rate. The number advances only
/// once the stream has opened.
pub(crate) async fn send(
    connection: &Connection,
    next_seq: &mut u64,
    job: &LatentJob,
    traffic: Option<&Counters>,
) -> Result<(), NetError> {
    let mut stream = connection.open_uni().await.map_err(|_| NetError::Closed)?;
    let seq = *next_seq;
    *next_seq = next_seq.saturating_add(1);
    let body = job.body.as_slice();
    let len = u32::try_from(body.len()).map_err(|_| NetError::Closed)?;
    let mut header = [0_u8; 1 + HEADER_LEN];
    header[0] = STREAM_KIND_LATENT;
    header[1..9].copy_from_slice(&seq.to_le_bytes());
    header[9..].copy_from_slice(&len.to_le_bytes());
    if stream.write_all(&header).await.is_err() {
        connection.close(VarInt::from_u32(LATENT_ABORTED), b"latent header failed");
        return Err(NetError::Closed);
    }
    if let Some(traffic) = traffic {
        traffic.sent_stream(header.len());
    }

    let rate = job.bytes_per_second;
    let chunk_len = usize::try_from((rate / CHUNKS_PER_SECOND).clamp(MIN_CHUNK_LEN, MAX_CHUNK_LEN))
        .unwrap_or(usize::MAX);
    let start = tokio::time::Instant::now();
    let mut sent = 0_usize;
    for chunk in body.chunks(chunk_len) {
        if stream.write_all(chunk).await.is_err() {
            return if matches!(stream.stopped().await, Ok(Some(_))) {
                Ok(())
            } else {
                Err(NetError::Closed)
            };
        }
        if let Some(traffic) = traffic {
            traffic.sent_data(chunk.len());
        }
        sent += chunk.len();
        tokio::time::sleep_until(start + due_after(sent, rate)).await;
    }
    if stream.finish().is_err() {
        return if matches!(stream.stopped().await, Ok(Some(_))) {
            Ok(())
        } else {
            Err(NetError::Closed)
        };
    }
    stream
        .stopped()
        .await
        .map(|_| ())
        .map_err(|_| NetError::Closed)
}

/// The time at which `sent` bytes are due at `rate` bytes per second, counted
/// from the start of the body. `rate` is at least [`MIN_LATENT_RATE`], so the
/// division is safe.
fn due_after(sent: usize, rate: u64) -> Duration {
    let sent = u128::try_from(sent).unwrap_or(u128::MAX);
    let nanos = sent.saturating_mul(1_000_000_000) / u128::from(rate);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Splits the header of a latent stream, which follows its kind byte, into the
/// event's sequence number and the length of its body.
pub(crate) fn parse_latent_header(header: &[u8; HEADER_LEN]) -> (u64, usize) {
    let mut seq = [0_u8; 8];
    seq.copy_from_slice(&header[..8]);
    let mut len = [0_u8; 4];
    len.copy_from_slice(&header[8..]);
    (
        u64::from_le_bytes(seq),
        usize::try_from(u32::from_le_bytes(len)).unwrap_or(usize::MAX),
    )
}

/// Reads the kind byte of a unidirectional stream. `None` when the stream ends
/// or fails before it.
pub(crate) async fn read_kind(recv: &mut RecvStream) -> Option<u8> {
    let mut kind = [0_u8; 1];
    tokio::time::timeout(LATENT_IDLE_TIMEOUT, recv.read_exact(&mut kind))
        .await
        .ok()?
        .ok()?;
    Some(kind[0])
}

/// What a receiving task hands to the connection's sequencer: the sequence
/// number of a stream and its event, or `None` when the stream was aborted, so
/// that the sequence can move on past it.
#[derive(Debug)]
pub(crate) struct Arrival {
    seq: u64,
    event: Option<Latent>,
}

/// A received latent event. It holds the stream's permit until the event has
/// been delivered, which bounds how many events wait in memory.
#[derive(Debug)]
pub(crate) struct Latent {
    _permit: OwnedSemaphorePermit,
    pub(crate) name: String,
    pub(crate) args: Vec<Value>,
}

/// Receives a latent stream whose kind byte the caller has read. Refuses the
/// stream when [`MAX_LATENT_STREAMS`] are already in progress, and aborts it when
/// its body is malformed, too large, too slow or followed by more data. The
/// stream's sequence and decoded event are returned once the body completes.
pub(crate) async fn receive(
    mut recv: RecvStream,
    slots: Arc<Semaphore>,
    traffic: Option<Arc<Counters>>,
) -> Option<Arrival> {
    let mut header = [0_u8; HEADER_LEN];
    match tokio::time::timeout(LATENT_IDLE_TIMEOUT, recv.read_exact(&mut header)).await {
        Ok(Ok(())) => {}
        _ => return None,
    }
    let (seq, len) = parse_latent_header(&header);
    if let Some(traffic) = &traffic {
        traffic.received_stream(1 + HEADER_LEN);
    }
    let Ok(permit) = slots.try_acquire_owned() else {
        debug!("a latent stream beyond the limit of {MAX_LATENT_STREAMS} was refused");
        let _ = recv.stop(VarInt::from_u32(LATENT_REFUSED));
        return Some(Arrival { seq, event: None });
    };
    let event = match read_body(&mut recv, len, traffic.as_deref())
        .await
        .and_then(|body| decode_latent_body(&body))
    {
        Ok((name, args)) => Some(Latent {
            _permit: permit,
            name,
            args: args.0,
        }),
        Err(err) => {
            debug!(%err, "a latent event was aborted");
            let _ = recv.stop(VarInt::from_u32(LATENT_ABORTED));
            None
        }
    };
    Some(Arrival { seq, event })
}

/// Reads a body of `len` bytes, then checks that the stream ends right after
/// it. The whole read must finish within [`budget`].
async fn read_body(
    recv: &mut RecvStream,
    len: usize,
    traffic: Option<&Counters>,
) -> Result<Vec<u8>, NetError> {
    if len > MAX_LATENT_EVENT_LEN {
        return Err(NetError::FrameTooLarge {
            len,
            max: MAX_LATENT_EVENT_LEN,
        });
    }
    let deadline = tokio::time::Instant::now() + budget(len);
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    let mut chunk = vec![0_u8; len.min(64 * 1024)];
    while body.len() < len {
        let want = chunk.len().min(len - body.len());
        let inactivity_deadline = tokio::time::Instant::now() + LATENT_IDLE_TIMEOUT;
        match tokio::time::timeout_at(
            deadline.min(inactivity_deadline),
            recv.read(&mut chunk[..want]),
        )
        .await
        {
            Ok(Ok(None | Some(0)) | Err(_)) => return Err(NetError::Closed),
            Ok(Ok(Some(read))) => {
                body.extend_from_slice(&chunk[..read]);
                if let Some(traffic) = traffic {
                    traffic.received_data(read);
                }
            }
            Err(_) => return Err(NetError::Timeout),
        }
    }
    let mut probe = [0_u8; 1];
    match tokio::time::timeout_at(
        deadline.min(tokio::time::Instant::now() + LATENT_IDLE_TIMEOUT),
        recv.read(&mut probe),
    )
    .await
    {
        Ok(Ok(None)) => Ok(body),
        Ok(Ok(Some(_))) => Err(NetError::Protocol(
            "a latent stream sent more than it announced".to_owned(),
        )),
        Ok(Err(_)) => Err(NetError::Closed),
        Err(_) => Err(NetError::Timeout),
    }
}

/// Time a stream with a body of `len` bytes may take: its size at the lowest
/// rate, rounded up, plus a grace period.
fn budget(len: usize) -> Duration {
    let bytes = u64::try_from(len).unwrap_or(u64::MAX);
    let seconds = bytes.saturating_add(MIN_LATENT_RATE - 1) / MIN_LATENT_RATE;
    Duration::from_secs(seconds).saturating_add(LATENT_GRACE)
}

/// Releases latent events in sequence order. An event that arrives early
/// waits for the ones before it.
#[derive(Debug, Default)]
pub(crate) struct Sequencer {
    next: u64,
    waiting: BTreeMap<u64, Option<Latent>>,
}

impl Sequencer {
    /// Takes in one arrival and returns the events that are now in order.
    /// Fails when more streams wait than can be in progress, which only a peer
    /// that skips sequence numbers can cause.
    pub(crate) fn push(&mut self, arrival: Arrival) -> Result<Vec<Latent>, NetError> {
        let Arrival { seq, event } = arrival;
        if seq < self.next || self.waiting.contains_key(&seq) {
            return Ok(Vec::new());
        }
        if seq > self.next && self.waiting.len() >= MAX_LATENT_STREAMS {
            return Err(NetError::Protocol(
                "latent streams arrived out of order".to_owned(),
            ));
        }
        self.waiting.insert(seq, event);
        let mut ready = Vec::new();
        while let Some(event) = self.waiting.remove(&self.next) {
            self.next = self.next.saturating_add(1);
            ready.extend(event);
        }
        Ok(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(seq: u64, name: &str, permit: Option<OwnedSemaphorePermit>) -> Arrival {
        let event = permit.map(|permit| Latent {
            _permit: permit,
            name: name.to_owned(),
            args: Vec::new(),
        });
        Arrival { seq, event }
    }

    #[test]
    fn events_are_released_in_sequence_order() {
        let slots = Arc::new(Semaphore::new(MAX_LATENT_STREAMS));
        let mut sequencer = Sequencer::default();
        let early = sequencer
            .push(arrival(1, "b", slots.clone().try_acquire_owned().ok()))
            .expect("accepted");
        assert!(early.is_empty());
        let ready = sequencer
            .push(arrival(0, "a", slots.clone().try_acquire_owned().ok()))
            .expect("accepted");
        let names: Vec<&str> = ready.iter().map(|event| event.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn an_aborted_stream_lets_the_sequence_move_on() {
        let mut sequencer = Sequencer::default();
        assert!(
            sequencer
                .push(arrival(0, "", None))
                .expect("accepted")
                .is_empty()
        );
        let slots = Arc::new(Semaphore::new(MAX_LATENT_STREAMS));
        let ready = sequencer
            .push(arrival(1, "next", slots.try_acquire_owned().ok()))
            .expect("accepted");
        assert_eq!(ready.len(), 1);
    }

    #[test]
    fn too_many_early_streams_are_a_protocol_error() {
        let slots = Arc::new(Semaphore::new(MAX_LATENT_STREAMS));
        let mut sequencer = Sequencer::default();
        for seq in 1..=MAX_LATENT_STREAMS as u64 {
            sequencer
                .push(arrival(seq, "", None))
                .expect("the first ones wait");
        }
        assert!(sequencer.push(arrival(99, "", None)).is_err());
        drop(slots);
    }

    #[test]
    fn budget_covers_full_transfer_at_minimum_rate() {
        assert_eq!(budget(0), LATENT_GRACE);
        assert_eq!(
            budget(1024 * 1024),
            Duration::from_secs(1024) + LATENT_GRACE
        );
        assert_eq!(
            budget(MAX_LATENT_EVENT_LEN),
            Duration::from_secs(16_384) + LATENT_GRACE
        );
    }

    #[test]
    fn rate_is_clamped_to_the_allowed_range() {
        assert_eq!(pacing_rate(0), 25_000);
        assert_eq!(pacing_rate(1), MIN_LATENT_RATE);
        assert_eq!(pacing_rate(u64::MAX), MAX_LATENT_RATE);
    }
}
