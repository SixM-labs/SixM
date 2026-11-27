//! Deterministic fuzzing of the wire decoders, run by `cargo test`.
//!
//! A seeded xorshift generator produces the inputs, so every run sees the same
//! 100,000 cases. Half are random bytes; the other half are valid frames
//! mutated by bit flips, truncation, insertion, splicing and oversized length
//! prefixes. Each input goes through every decoder, which must return an error
//! or a message that respects the limits. A panic fails the test.

use sixm_core::Value;

use crate::entities::EntityEntry;
use crate::files::{FILE_KIND_FILE, FileEntry, ResourceEntry};
use crate::protocol::{
    ClientMessage, MAX_COMMAND_LEN, MAX_DECODED_VALUES, MAX_FRAME, MAX_TRANSFORMS_PER_MESSAGE,
    PROTOCOL_VERSION, ServerMessage,
};
use crate::wire::{
    Args, FRAME_HEADER_LEN, StateValue, decode_client_message, decode_latent_body,
    decode_server_message, encode_frame, encode_latent_body,
};

/// Inputs per run. Each input goes through every decoder.
const CASES: usize = 100_000;
/// Longest random input in bytes.
const MAX_RANDOM_LEN: usize = 256;
/// Bytes that often mark a boundary in the wire format: tags, lengths, varints.
const INTERESTING: [u8; 8] = [0x00, 0x01, 0x02, 0x05, 0x7f, 0x80, 0xfe, 0xff];

/// Seeded xorshift64 generator. The fixed seed keeps the test reproducible.
struct Xorshift(u64);

impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number in `0..n`, or 0 when `n` is 0.
    fn below(&mut self, n: usize) -> usize {
        match u64::try_from(n) {
            Ok(n) if n > 0 => usize::try_from(self.next_u64() % n).unwrap_or(0),
            _ => 0,
        }
    }

    fn byte(&mut self) -> u8 {
        self.next_u64().to_le_bytes()[0]
    }
}

fn sample_values() -> Vec<Value> {
    vec![
        Value::Nil,
        Value::Bool(true),
        Value::Int(-42),
        Value::Float(1.5),
        Value::String(b"text".to_vec().into_boxed_slice()),
        Value::Array(vec![Value::Int(1), Value::Array(vec![Value::Nil])]),
        Value::Map(vec![(
            Value::String(b"key".to_vec().into_boxed_slice()),
            Value::Vector3([1.0, 2.0, 3.0]),
        )]),
        Value::Vector2([0.5, 0.25]),
        Value::Vector4([1.0, 0.0, 0.0, 1.0]),
    ]
}

fn sample_entity() -> EntityEntry {
    EntityEntry {
        id: 7,
        kind: 2,
        model: 1_234,
        position: [1.0, 2.0, 3.0],
        rotation: [0.0, 90.0, 0.0],
        owner: 1,
        epoch: 3,
    }
}

fn sample_resource() -> ResourceEntry {
    ResourceEntry {
        name: "core".to_owned(),
        files: vec![FileEntry {
            name: "client/main.lua".to_owned(),
            kind: FILE_KIND_FILE,
            hash: [9; 32],
            size: 512,
        }],
        exports: vec!["getCore".to_owned()],
        metadata: vec![("author".to_owned(), "tester".to_owned())],
    }
}

/// The payload of a frame: the frame without its length prefix.
fn payload_of<T: serde::Serialize>(message: &T) -> Vec<u8> {
    let frame = encode_frame(message, MAX_FRAME).expect("seed message encodes");
    frame[FRAME_HEADER_LEN..].to_vec()
}

/// Valid payloads of every client message, used as mutation seeds.
fn client_seeds() -> Vec<Vec<u8>> {
    let transforms = vec![
        (3, [1.0, 2.0, 3.0], [0.0, 90.0, 0.0], 2),
        (4, [0.0; 3], [0.0; 3], 0),
    ];
    let seeds: [Vec<u8>; 5] = [
        payload_of(&ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            name: "player".to_owned(),
        }),
        payload_of(&ClientMessage::Event {
            name: "chat:message".to_owned(),
            args: Args(sample_values()),
        }),
        payload_of(&ClientMessage::RequestFile { hash: [7; 32] }),
        payload_of(&ClientMessage::Command {
            line: "status".to_owned(),
        }),
        payload_of(&ClientMessage::EntityTransforms {
            updates: transforms,
        }),
    ];
    Vec::from(seeds)
}

/// Valid payloads of every server message, used as mutation seeds.
fn server_seeds() -> Vec<Vec<u8>> {
    let convars = vec![
        ("sv_name".to_owned(), Some("SixM".to_owned())),
        ("removed".to_owned(), None),
    ];
    vec![
        payload_of(&ServerMessage::Welcome {
            player: 1,
            server_name: "SixM".to_owned(),
        }),
        payload_of(&ServerMessage::Rejected {
            reason: "full".to_owned(),
        }),
        payload_of(&ServerMessage::Event {
            name: "chat:message".to_owned(),
            args: Args(sample_values()),
        }),
        payload_of(&ServerMessage::Kicked {
            reason: "bye".to_owned(),
        }),
        payload_of(&ServerMessage::StateBag {
            bag: "global".to_owned(),
            key: "score".to_owned(),
            value: StateValue(Value::Map(vec![(Value::Int(1), Value::Nil)])),
        }),
        payload_of(&ServerMessage::StateBagRemoved {
            bag: "player:4".to_owned(),
        }),
        payload_of(&ServerMessage::StateSnapshot {
            entries: vec![(
                "global".to_owned(),
                "key".to_owned(),
                StateValue(Value::Array(vec![Value::Bool(false)])),
            )],
        }),
        payload_of(&ServerMessage::Resources {
            entries: vec![sample_resource()],
        }),
        payload_of(&ServerMessage::ResourceStarted {
            entry: sample_resource(),
        }),
        payload_of(&ServerMessage::ResourceStopped {
            name: "core".to_owned(),
        }),
        payload_of(&ServerMessage::FileUnavailable { hash: [1; 32] }),
        payload_of(&ServerMessage::EntityCreated {
            entity: sample_entity(),
        }),
        payload_of(&ServerMessage::EntityTransforms {
            updates: vec![(7, [1.0; 3], [2.0; 3], 3)],
        }),
        payload_of(&ServerMessage::EntityDeleted { id: 7 }),
        payload_of(&ServerMessage::EntitySnapshot {
            entities: vec![sample_entity()],
        }),
        payload_of(&ServerMessage::Convars { entries: convars }),
        payload_of(&ServerMessage::Deferral {
            message: "waiting".to_owned(),
        }),
    ]
}

/// Valid body of a latent event.
fn latent_seed() -> Vec<u8> {
    encode_latent_body("bulk:data", &sample_values()).expect("latent seed encodes")
}

/// Copies `seed` and applies one to four random edits. `other` supplies bytes
/// for splicing.
fn mutate(rng: &mut Xorshift, seed: &[u8], other: &[u8]) -> Vec<u8> {
    let mut data = seed.to_vec();
    for _ in 0..=rng.below(3) {
        match rng.below(8) {
            0 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
            1 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] = rng.byte();
            }
            2 => {
                let keep = rng.below(data.len() + 1);
                data.truncate(keep);
            }
            3 => {
                for _ in 0..=rng.below(15) {
                    data.push(rng.byte());
                }
            }
            4 => {
                let i = rng.below(data.len() + 1);
                data.insert(i, rng.byte());
            }
            5 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] = INTERESTING[rng.below(INTERESTING.len())];
            }
            6 => {
                let from = rng.below(data.len() + 1);
                let cut = rng.below(other.len() + 1);
                data.truncate(from);
                data.extend_from_slice(&other[cut..]);
            }
            _ => {
                // A length prefix of about 2^35 elements, far beyond any frame.
                let i = rng.below(data.len() + 1);
                data.splice(i..i, [0xff, 0xff, 0xff, 0xff, 0x7f]);
            }
        }
    }
    data
}

/// Counts the values in `values`, nested ones included.
fn value_count(values: &[Value]) -> usize {
    values
        .iter()
        .map(|value| match value {
            Value::Array(items) => 1 + value_count(items),
            Value::Map(entries) => {
                1 + entries
                    .iter()
                    .map(|(key, value)| {
                        value_count(std::slice::from_ref(key))
                            + value_count(std::slice::from_ref(value))
                    })
                    .sum::<usize>()
            }
            _ => 1,
        })
        .sum()
}

/// Feeds one input to every decoder. An error is fine; a decoded message must
/// respect the limits that its decoder enforces.
fn check_input(data: &[u8]) -> usize {
    let mut accepted = 0;
    if let Ok(message) = decode_client_message(data) {
        accepted += 1;
        match message {
            ClientMessage::Event { args, .. } => {
                assert!(value_count(&args.0) <= MAX_DECODED_VALUES);
            }
            ClientMessage::Command { line } => assert!(line.len() <= MAX_COMMAND_LEN),
            ClientMessage::EntityTransforms { updates } => {
                assert!(updates.len() <= MAX_TRANSFORMS_PER_MESSAGE);
            }
            ClientMessage::Hello { .. } | ClientMessage::RequestFile { .. } => {}
        }
    }
    if let Ok(message) = decode_server_message(data) {
        accepted += 1;
        match message {
            ServerMessage::Event { args, .. } => {
                assert!(value_count(&args.0) <= MAX_DECODED_VALUES);
            }
            ServerMessage::StateBag { value, .. } => {
                assert!(value_count(std::slice::from_ref(&value.0)) <= MAX_DECODED_VALUES);
            }
            ServerMessage::StateSnapshot { entries } => {
                assert!(entries.len() <= MAX_DECODED_VALUES);
            }
            ServerMessage::EntityTransforms { updates } => {
                assert!(updates.len() <= MAX_DECODED_VALUES);
            }
            _ => {}
        }
    }
    if let Ok((_, Args(args))) = decode_latent_body(data) {
        accepted += 1;
        assert!(value_count(&args) <= MAX_DECODED_VALUES);
    }
    accepted
}

#[test]
fn seeds_decode_with_their_own_decoder() {
    for payload in client_seeds() {
        decode_client_message(&payload).expect("client seed decodes");
    }
    for payload in server_seeds() {
        decode_server_message(&payload).expect("server seed decodes");
    }
    decode_latent_body(&latent_seed()).expect("latent seed decodes");
}

#[test]
fn hundred_thousand_random_and_mutated_inputs_never_panic() {
    let client = client_seeds();
    let server = server_seeds();
    let latent = latent_seed();
    let seeds: Vec<&Vec<u8>> = client.iter().chain(server.iter()).collect();
    let mut rng = Xorshift(0x5eed_5eed_1234_5678);

    let mut accepted = 0;
    for case in 0..CASES {
        let data = if case % 2 == 0 {
            let len = rng.below(MAX_RANDOM_LEN + 1);
            (0..len).map(|_| rng.byte()).collect()
        } else {
            let seed = seeds[rng.below(seeds.len())];
            let other = if rng.below(4) == 0 {
                &latent
            } else {
                seeds[rng.below(seeds.len())]
            };
            mutate(&mut rng, seed, other)
        };
        accepted += check_input(&data);
    }
    // Most inputs are rejected, as they should be. Mutated seeds must still
    // reach the accepting paths often enough, or the test would be weak.
    assert!(accepted > CASES / 50, "only {accepted} decodes succeeded");
}

#[test]
fn huge_sequence_length_is_rejected_without_allocating_it() {
    // An event named "a" whose argument list claims about 2^63 values but
    // carries none: the decoder must fail on the missing data.
    let mut payload = vec![1, 1, b'a'];
    payload.extend_from_slice(&[0xff; 9]);
    payload.push(0x01);
    assert!(decode_client_message(&payload).is_err());
    assert!(decode_latent_body(&payload[1..]).is_err());
}
