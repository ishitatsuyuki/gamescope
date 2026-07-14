//! Refresh-rate conversions ported from `src/refresh_rate.h`.

/// Convert integer hertz to millihertz.
#[must_use]
pub const fn hertz_to_millihertz(refresh_hz: u32) -> u32 {
    refresh_hz * 1_000
}

/// Convert integer millihertz to hertz using Gamescope's rounding rule.
///
/// The `+499` is intentional and matches the C++ implementation.
#[must_use]
pub const fn millihertz_to_hertz(refresh_millihz: u32) -> u32 {
    (refresh_millihz + 499) / 1_000
}

/// Convert a refresh cycle in nanoseconds to millihertz.
///
/// Returns `None` for a zero cycle rather than invoking undefined arithmetic.
#[must_use]
pub const fn refresh_cycle_to_millihertz(cycle_ns: u64) -> Option<u32> {
    if cycle_ns == 0 {
        return None;
    }

    let rounded = (1_000_000_000_000_u64 + cycle_ns / 2 - 1) / cycle_ns;
    if rounded > u32::MAX as u64 {
        None
    } else {
        Some(rounded as u32)
    }
}

/// Convert millihertz to a refresh cycle in nanoseconds.
#[must_use]
pub const fn millihertz_to_refresh_cycle(refresh_millihz: u32) -> Option<u64> {
    if refresh_millihz == 0 {
        return None;
    }

    let rate = refresh_millihz as u64;
    Some((1_000_000_000_000_u64 + rate / 2 - 1) / rate)
}

#[cfg(test)]
mod tests {
    use super::{
        hertz_to_millihertz, millihertz_to_hertz, millihertz_to_refresh_cycle,
        refresh_cycle_to_millihertz,
    };

    #[test]
    fn integer_rate_conversions_match_gamescope() {
        assert_eq!(hertz_to_millihertz(60), 60_000);
        assert_eq!(millihertz_to_hertz(60_001), 60);
        assert_eq!(millihertz_to_hertz(143_990), 144);
    }

    #[test]
    fn cycle_conversion_round_trips_common_rates() {
        for rate in [24_000, 30_000, 59_940, 60_000, 90_000, 120_000, 144_000] {
            let cycle = millihertz_to_refresh_cycle(rate).unwrap();
            assert_eq!(refresh_cycle_to_millihertz(cycle), Some(rate));
        }
    }

    #[test]
    fn zero_has_no_finite_cycle() {
        assert_eq!(refresh_cycle_to_millihertz(0), None);
        assert_eq!(millihertz_to_refresh_cycle(0), None);
    }
}
