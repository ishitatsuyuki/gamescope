//! Helpers for Wayland arguments that split 64-bit values into 32-bit words.

/// Joins a high and low 32-bit word in that order.
#[must_use]
pub const fn join_u64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// Splits a value into `(high, low)` words.
#[must_use]
pub const fn split_u64(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

#[cfg(test)]
mod tests {
    use super::{join_u64, split_u64};

    #[test]
    fn split_and_join_are_inverses() {
        for value in [
            0,
            1,
            u64::from(u32::MAX),
            1_u64 << 32,
            0x0123_4567_89ab_cdef,
            u64::MAX,
        ] {
            let (high, low) = split_u64(value);
            assert_eq!(join_u64(high, low), value);
        }
    }
}
