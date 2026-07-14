//! Reduced unsigned ratios matching the behavior of `src/Ratio.h`.

use std::fmt;

/// A reduced ratio. A denominator of zero denotes an undefined ratio.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Ratio {
    numerator: u32,
    denominator: u32,
}

impl Ratio {
    /// Construct and reduce a ratio.
    #[must_use]
    pub const fn new(numerator: u32, denominator: u32) -> Self {
        let divisor = gcd(numerator, denominator);
        match (
            numerator.checked_div(divisor),
            denominator.checked_div(divisor),
        ) {
            (Some(numerator), Some(denominator)) => Self {
                numerator,
                denominator,
            },
            _ => Self {
                numerator: 0,
                denominator: 0,
            },
        }
    }

    /// Parse using Gamescope's permissive `from_chars` behavior.
    ///
    /// Missing or malformed components become zero. A missing colon produces
    /// the undefined `0:0` value.
    #[must_use]
    pub fn parse_gamescope(value: &str) -> Self {
        let Some((numerator, denominator)) = value.split_once(':') else {
            return Self::default();
        };

        Self::new(
            numerator.parse().unwrap_or(0),
            denominator.parse().unwrap_or(0),
        )
    }

    /// Reduced numerator.
    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    /// Reduced denominator.
    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.denominator
    }

    /// Whether this ratio has no finite denominator.
    #[must_use]
    pub const fn is_undefined(self) -> bool {
        self.denominator == 0
    }

    /// Compare two defined ratios without converting to floating point.
    #[must_use]
    pub const fn compare(self, other: Self) -> Option<std::cmp::Ordering> {
        if self.is_undefined() || other.is_undefined() {
            return None;
        }

        let left = self.numerator as u64 * other.denominator as u64;
        let right = other.numerator as u64 * self.denominator as u64;
        if left < right {
            Some(std::cmp::Ordering::Less)
        } else if left > right {
            Some(std::cmp::Ordering::Greater)
        } else {
            Some(std::cmp::Ordering::Equal)
        }
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.numerator, self.denominator)
    }
}

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::Ratio;

    #[test]
    fn reduces_ratios() {
        assert_eq!(Ratio::new(3840, 2160), Ratio::new(16, 9));
        assert_eq!(Ratio::new(0, 4), Ratio::new(0, 1));
    }

    #[test]
    fn parser_matches_permissive_cpp_behavior() {
        assert_eq!(Ratio::parse_gamescope("16:9"), Ratio::new(16, 9));
        assert_eq!(Ratio::parse_gamescope("bad:9"), Ratio::new(0, 1));
        assert!(Ratio::parse_gamescope("16:bad").is_undefined());
        assert!(Ratio::parse_gamescope("16/9").is_undefined());
    }

    #[test]
    fn compares_without_float_rounding() {
        assert_eq!(
            Ratio::new(16, 9).compare(Ratio::new(4, 3)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Ratio::new(8, 6).compare(Ratio::new(4, 3)),
            Some(Ordering::Equal)
        );
        assert_eq!(Ratio::default().compare(Ratio::new(4, 3)), None);
    }
}
