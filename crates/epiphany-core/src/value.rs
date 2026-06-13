//! Exact fixed-point numeric values (ADR-0008).
//!
//! Cell values use a scaled 64-bit integer instead of floating point, so
//! arithmetic is exact and deterministic — no float rounding and no
//! summation-order effects — while a value stays 8 bytes. The scale is `10^4`,
//! i.e. four decimal places.

use std::fmt;

use crate::ModelError;

/// Decimal places of precision.
pub const SCALE_DECIMALS: u32 = 4;
/// Scale factor: a stored value equals the real value multiplied by `SCALE`.
pub const SCALE: i64 = 10_000;

/// An exact fixed-point number, stored as `value × 10^4` in an `i64`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Fixed(i64);

impl Fixed {
    /// The zero value.
    pub const ZERO: Fixed = Fixed(0);

    /// Wrap a raw scaled integer (already multiplied by [`SCALE`]).
    pub const fn from_scaled(scaled: i64) -> Self {
        Fixed(scaled)
    }

    /// The underlying scaled integer.
    pub const fn to_scaled(self) -> i64 {
        self.0
    }

    /// Build from a whole integer; errors on overflow.
    pub fn from_int(n: i64) -> Result<Self, ModelError> {
        n.checked_mul(SCALE).map(Fixed).ok_or(ModelError::Overflow)
    }

    /// `true` if this is exactly zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl From<i32> for Fixed {
    fn from(n: i32) -> Self {
        // i32 × 10_000 always fits in i64, so this is infallible.
        Fixed(n as i64 * SCALE)
    }
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let scale = SCALE as u64;
        let int = abs / scale;
        let frac = abs % scale;
        if frac == 0 {
            write!(f, "{sign}{int}")
        } else {
            let mut frac_str = format!("{:0width$}", frac, width = SCALE_DECIMALS as usize);
            while frac_str.ends_with('0') {
                frac_str.pop();
            }
            write!(f, "{sign}{int}.{frac_str}")
        }
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fixed({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_int_scales() {
        assert_eq!(Fixed::from_int(5).unwrap().to_scaled(), 50_000);
        assert_eq!(Fixed::from(5), Fixed::from_int(5).unwrap());
    }

    #[test]
    fn display_is_canonical() {
        assert_eq!(Fixed::from(0).to_string(), "0");
        assert_eq!(Fixed::from(5).to_string(), "5");
        assert_eq!(Fixed::from(-5).to_string(), "-5");
        assert_eq!(Fixed::from_scaled(12_345).to_string(), "1.2345");
        assert_eq!(Fixed::from_scaled(10_500).to_string(), "1.05");
        assert_eq!(Fixed::from_scaled(1_000).to_string(), "0.1");
        assert_eq!(Fixed::from_scaled(-22_500).to_string(), "-2.25");
        assert_eq!(Fixed::from_scaled(1).to_string(), "0.0001");
    }

    #[test]
    fn zero_helpers() {
        assert!(Fixed::ZERO.is_zero());
        assert!(!Fixed::from(1).is_zero());
    }

    #[test]
    fn from_int_overflows_cleanly() {
        assert_eq!(Fixed::from_int(i64::MAX), Err(ModelError::Overflow));
    }
}
