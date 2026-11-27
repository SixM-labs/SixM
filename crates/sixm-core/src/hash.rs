//! Name hashing shared by the runtimes and the game adapters.

/// Computes Bob Jenkins' one-at-a-time hash of `text`, after lowercasing its
/// ASCII letters. Games use it for model and other asset names, and scripts
/// reach it through `GetHashKey`.
///
/// The arithmetic wraps at 32 bits, so the result is the same on every
/// platform.
pub fn joaat(text: &[u8]) -> u32 {
    let mut hash = 0_u32;
    for byte in text {
        hash = hash.wrapping_add(u32::from(byte.to_ascii_lowercase()));
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

#[cfg(test)]
mod tests {
    use super::joaat;

    #[test]
    fn known_name_hashes() {
        assert_eq!(joaat(b"adder"), 0xB779_A091);
        assert_eq!(joaat(b""), 0);
    }

    #[test]
    fn hashing_ignores_ascii_case() {
        assert_eq!(joaat(b"ADDER"), joaat(b"adder"));
        assert_eq!(joaat(b"Sultan_RS"), joaat(b"sultan_rs"));
    }

    #[test]
    fn different_names_differ() {
        assert_ne!(joaat(b"adder"), joaat(b"sultan"));
    }
}
