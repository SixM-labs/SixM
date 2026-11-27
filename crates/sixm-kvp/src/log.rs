//! The log file format of a resource's key-value store.
//!
//! A log is a sequence of records. Each record is
//!
//! ```text
//! tag        u8        1 = set, 2 = delete
//! key_len    u16 LE    1 to 256
//! value_len  u32 LE    up to 64 KiB, 0 for a delete
//! crc        u32 LE    CRC-32 (IEEE) of the first 7 bytes, the key and the value
//! key        key_len bytes of UTF-8
//! value      value_len bytes
//! ```
//!
//! Replaying the records in order gives the live entries. A truncated or
//! corrupt record ends the valid part of the log.

use std::collections::BTreeMap;

use crate::{MAX_KEY_LEN, MAX_VALUE_LEN};

pub(crate) const TAG_SET: u8 = 1;
pub(crate) const TAG_DELETE: u8 = 2;
/// Tag, key length, value length and CRC.
const HEADER_LEN: usize = 11;
/// Bytes of the header that the CRC does not cover: tag, key length, value length.
const CHECKED_HEADER_LEN: usize = 7;

/// The entries of a resource, in key order.
pub(crate) type Entries = BTreeMap<String, Vec<u8>>;

/// Applies the records of `bytes` to `entries` and returns the length of the
/// valid prefix of `bytes`.
pub(crate) fn replay_into(entries: &mut Entries, bytes: &[u8]) -> usize {
    let mut offset = 0;
    while let Some(record) = parse_record(&bytes[offset..]) {
        match record.tag {
            TAG_SET => {
                entries.insert(record.key.to_owned(), record.value.to_vec());
            }
            _ => {
                entries.remove(record.key);
            }
        }
        offset += record.len;
    }
    offset
}

struct Record<'a> {
    tag: u8,
    key: &'a str,
    value: &'a [u8],
    len: usize,
}

/// Parses the record at the start of `bytes`. `None` for a truncated, corrupt
/// or out-of-range record.
fn parse_record(bytes: &[u8]) -> Option<Record<'_>> {
    let header = bytes.get(..HEADER_LEN)?;
    let tag = header[0];
    if tag != TAG_SET && tag != TAG_DELETE {
        return None;
    }
    let key_len = usize::from(u16::from_le_bytes([header[1], header[2]]));
    let value_len = usize::try_from(u32::from_le_bytes([
        header[3], header[4], header[5], header[6],
    ]))
    .ok()?;
    let crc = u32::from_le_bytes([header[7], header[8], header[9], header[10]]);
    if !(1..=MAX_KEY_LEN).contains(&key_len)
        || value_len > MAX_VALUE_LEN
        || (tag == TAG_DELETE && value_len != 0)
    {
        return None;
    }
    let body = bytes.get(HEADER_LEN..HEADER_LEN + key_len + value_len)?;
    let (key, value) = body.split_at(key_len);
    if crc32(&[&header[..CHECKED_HEADER_LEN], key, value]) != crc {
        return None;
    }
    Some(Record {
        tag,
        key: std::str::from_utf8(key).ok()?,
        value,
        len: HEADER_LEN + key_len + value_len,
    })
}

/// Appends one record for `key` and `value` to `out`. The key and value must
/// be within their limits.
pub(crate) fn encode(out: &mut Vec<u8>, tag: u8, key: &str, value: &[u8]) {
    let key_len = u16::try_from(key.len()).expect("keys are checked to be at most 256 bytes");
    let value_len = u32::try_from(value.len()).expect("values are checked to be at most 64 KiB");
    let mut header = [0_u8; HEADER_LEN];
    header[0] = tag;
    header[1..3].copy_from_slice(&key_len.to_le_bytes());
    header[3..7].copy_from_slice(&value_len.to_le_bytes());
    let crc = crc32(&[&header[..CHECKED_HEADER_LEN], key.as_bytes(), value]);
    header[7..].copy_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(value);
}

/// Bytes one live entry takes in a log.
pub(crate) fn record_len(key: &str, value: &[u8]) -> u64 {
    len_u64(HEADER_LEN + key.len() + value.len())
}

pub(crate) fn len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// CRC-32 (IEEE 802.3, reflected) of the concatenation of `parts`.
fn crc32(parts: &[&[u8]]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    let mut crc = !0_u32;
    for part in parts {
        for &byte in *part {
            let index = usize::from(crc.to_le_bytes()[0] ^ byte);
            crc = TABLE[index] ^ (crc >> 8);
        }
    }
    !crc
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index: u32 = 0;
    while index < 256 {
        let mut value = index;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index as usize] = value;
        index += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::{Entries, TAG_DELETE, TAG_SET, crc32, encode, replay_into};

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"12345", b"6789"]), 0xCBF4_3926);
    }

    #[test]
    fn replay_applies_sets_and_deletes_in_order() {
        let mut log = Vec::new();
        encode(&mut log, TAG_SET, "a", b"1");
        encode(&mut log, TAG_SET, "b", b"2");
        encode(&mut log, TAG_SET, "a", b"3");
        encode(&mut log, TAG_DELETE, "b", &[]);
        let mut entries = Entries::new();
        assert_eq!(replay_into(&mut entries, &log), log.len());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries.get("a"), Some(&b"3".to_vec()));
    }

    #[test]
    fn replay_stops_before_a_truncated_or_corrupt_record() {
        let mut log = Vec::new();
        encode(&mut log, TAG_SET, "kept", b"yes");
        let good = log.len();
        encode(&mut log, TAG_SET, "lost", b"no");

        let mut entries = Entries::new();
        assert_eq!(replay_into(&mut entries, &log[..log.len() - 1]), good);
        assert!(entries.contains_key("kept") && !entries.contains_key("lost"));

        let last = log.len() - 1;
        log[last] ^= 0xFF;
        let mut entries = Entries::new();
        assert_eq!(replay_into(&mut entries, &log), good);
        assert!(!entries.contains_key("lost"));
    }
}
