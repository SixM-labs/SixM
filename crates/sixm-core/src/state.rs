//! Names of state bags, shared by the server and the script runtimes.

/// The bag of state shared by the whole server.
pub const GLOBAL_BAG: &str = "global";

/// Longest bag name or state key, in bytes.
pub const MAX_STATE_NAME_LEN: usize = 256;

/// Name of the bag that holds the state of player `player`.
pub fn player_bag(player: u32) -> String {
    format!("player:{player}")
}

/// The player a bag belongs to, or `None` when `bag` is not a player bag.
pub fn player_of_bag(bag: &str) -> Option<u32> {
    let digits = bag.strip_prefix("player:")?;
    let player: u32 = digits.parse().ok()?;
    // Only the canonical spelling names a bag, so `player:05` is not `player:5`.
    (player.to_string() == digits).then_some(player)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_bags_round_trip() {
        assert_eq!(player_bag(7), "player:7");
        assert_eq!(player_of_bag("player:7"), Some(7));
    }

    #[test]
    fn only_canonical_player_bags_name_a_player() {
        for bag in [
            "player:05",
            "player:+5",
            "player:",
            "player:x",
            "global",
            "player:-1",
        ] {
            assert_eq!(player_of_bag(bag), None, "{bag}");
        }
    }
}
