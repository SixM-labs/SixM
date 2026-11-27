//! Traffic counters of the server. Every connection and file transfer updates
//! the same counters, once per frame written or read. The counters are relaxed
//! atomics, so counting never allocates or locks.

use std::sync::atomic::{AtomicU64, Ordering};

/// Totals of the traffic that crossed the server's connections since it
/// started. Frame payloads and their length prefixes count, and so does file
/// data. QUIC packet overhead, handshakes and keep-alives do not.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Traffic {
    /// Bytes written to clients.
    pub bytes_sent: u64,
    /// Bytes read from clients.
    pub bytes_received: u64,
    /// Frames written to clients. A file transfer counts as one frame.
    pub frames_sent: u64,
    /// Frames read from clients.
    pub frames_received: u64,
}

/// The live counters behind [`Traffic`].
#[derive(Debug, Default)]
pub(crate) struct Counters {
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    frames_sent: AtomicU64,
    frames_received: AtomicU64,
}

impl Counters {
    /// Counts one frame of `bytes` written to a client.
    pub(crate) fn sent(&self, bytes: usize) {
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.sent_data(bytes);
    }

    /// Counts `bytes` written to a client as part of a frame that is counted
    /// already, such as the chunks of a file.
    pub(crate) fn sent_data(&self, bytes: usize) {
        self.bytes_sent.fetch_add(as_u64(bytes), Ordering::Relaxed);
    }

    /// Counts one frame of `bytes` read from a client.
    pub(crate) fn received(&self, bytes: usize) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received
            .fetch_add(as_u64(bytes), Ordering::Relaxed);
    }

    /// The totals so far.
    pub(crate) fn snapshot(&self) -> Traffic {
        Traffic {
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            frames_received: self.frames_received.load(Ordering::Relaxed),
        }
    }
}

fn as_u64(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Counters, Traffic};

    #[test]
    fn frames_and_bytes_are_counted_separately() {
        let counters = Counters::default();
        counters.sent(10);
        counters.sent_data(100);
        counters.received(7);
        assert_eq!(
            counters.snapshot(),
            Traffic {
                bytes_sent: 110,
                bytes_received: 7,
                frames_sent: 1,
                frames_received: 1,
            }
        );
    }
}
