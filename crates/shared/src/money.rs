//! C05 / M1–M3: the shared money type.
//!
//! `MoneyMicros` is a newtype over `u64` micro-USD (1 USD = 1_000_000 micros),
//! matching the storage floor of `NUMERIC(12,6)` (money-contract M1). It exists
//! so that:
//!
//! * **M2** — money is never carried as `f64` in accumulators. The only `f64`
//!   entry points are the two *biased* constructors, so a conversion always
//!   rounds in a caller-chosen, contract-defined direction rather than
//!   silently truncating:
//!     * [`MoneyMicros::from_usd_floor`] — round **down**. Use for a declared
//!       allowance/ceiling where rounding up would over-permit.
//!     * [`MoneyMicros::from_usd_ceil`] — round **up**. Use for an observed
//!       spend/charge where rounding down would under-report.
//! * **M3** — non-finite and negative inputs are rejected (`None`), never
//!   coerced to zero. "Unavailable is never measured zero."
//! * **D3** — equality is integer equality; the `f64` `==` comparison class is
//!   removed at the boundary.
//!
//! This module is deliberately pure and dependency-free so every crate can
//! adopt it. It does not itself migrate any column or call site (that is the
//! staged C05 rollout in the money contract); it is the shared primitive those
//! migrations target.

use std::fmt;

/// Micro-USD per USD. Matches the `NUMERIC(12,6)` storage scale.
pub const MICRO_USD_PER_USD: f64 = 1_000_000.0;

/// A non-negative monetary amount in micro-USD.
///
/// Copy + `Ord` so it can be summed and compared without float drift. `u64`
/// bounds a single amount at ~1.8e13 USD — far above any request, monthly, or
/// organizational figure this system carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct MoneyMicros(u64);

/// Why a `f64` could not become a `MoneyMicros`. Value-free: the offending
/// number is deliberately not retained in the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyConversionError {
    /// The value was NaN or infinite.
    NotFinite,
    /// The value was negative.
    Negative,
    /// The value exceeded the `u64` micro-USD range.
    Overflow,
}

impl fmt::Display for MoneyConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite => write!(f, "money value is not finite"),
            Self::Negative => write!(f, "money value is negative"),
            Self::Overflow => write!(f, "money value exceeds the micro-USD range"),
        }
    }
}

impl std::error::Error for MoneyConversionError {}

impl MoneyMicros {
    /// Zero micro-USD. The honest zero (M3): only ever produced explicitly,
    /// never as a fallback for an unavailable figure.
    pub const ZERO: Self = Self(0);

    /// Construct from an exact micro-USD count.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// The raw micro-USD count.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// **M3 biased constructor — round down.** For a declared allowance or
    /// ceiling, rounding up would over-permit, so the conservative direction is
    /// toward zero.
    ///
    /// Rejects non-finite, negative, and out-of-range inputs with `None`
    /// (M3: never coerced to zero).
    #[must_use]
    pub fn from_usd_floor(value: f64) -> Option<Self> {
        Self::scale(value, f64::floor)
    }

    /// **M3 biased constructor — round up.** For an observed spend or charge,
    /// rounding down would under-report, so the conservative direction is
    /// away from zero.
    ///
    /// Rejects non-finite, negative, and out-of-range inputs with `None`.
    #[must_use]
    pub fn from_usd_ceil(value: f64) -> Option<Self> {
        Self::scale(value, f64::ceil)
    }

    fn scale(value: f64, round: fn(f64) -> f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        if value < 0.0 {
            return None;
        }
        let scaled = value * MICRO_USD_PER_USD;
        if scaled > u64::MAX as f64 {
            return None;
        }
        Some(Self(round(scaled) as u64))
    }

    /// Checked addition — an overflow is a hard error, never a silent wrap.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Checked subtraction — a negative result is a hard error (money is
    /// non-negative here; a signed delta is a separate concept).
    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    /// The value as exact `f64` USD for display. Exact because every micro-USD
    /// integer under 2^53 is representable; the shared display formatters (M9)
    /// are the sanctioned presentation path.
    #[must_use]
    pub fn as_usd_f64(self) -> f64 {
        self.0 as f64 / MICRO_USD_PER_USD
    }
}

impl fmt::Display for MoneyMicros {
    /// Canonical display: USD with exactly 6 decimals (the storage floor, M1).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.6}", self.as_usd_f64())
    }
}

impl std::iter::Sum for MoneyMicros {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        Self(iter.map(|m| m.0).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biased_constructors_round_in_the_declared_direction() {
        // 1.0000005 USD = 1_000_000.5 micros — the floor/ceil split.
        let floor = MoneyMicros::from_usd_floor(1.0000005).unwrap();
        let ceil = MoneyMicros::from_usd_ceil(1.0000005).unwrap();
        assert_eq!(floor.as_micros(), 1_000_000);
        assert_eq!(ceil.as_micros(), 1_000_001);
        assert!(floor < ceil, "the biased directions must differ on a half");
    }

    #[test]
    fn non_finite_negative_and_overflow_are_rejected_never_zeroed() {
        // M3: every failure is None, NOT MoneyMicros::ZERO.
        assert_eq!(MoneyMicros::from_usd_floor(f64::NAN), None);
        assert_eq!(MoneyMicros::from_usd_ceil(f64::INFINITY), None);
        assert_eq!(MoneyMicros::from_usd_floor(-0.01), None);
        assert_eq!(MoneyMicros::from_usd_ceil(-1.0), None);
        assert_eq!(MoneyMicros::from_usd_floor(f64::MAX), None);
    }

    #[test]
    fn zero_is_explicit_and_exact() {
        assert_eq!(MoneyMicros::from_usd_floor(0.0), Some(MoneyMicros::ZERO));
        assert_eq!(MoneyMicros::from_usd_ceil(0.0), Some(MoneyMicros::ZERO));
        assert_eq!(MoneyMicros::ZERO.as_micros(), 0);
    }

    #[test]
    fn integer_arithmetic_is_exact_and_checked() {
        let a = MoneyMicros::from_micros(1_000_000);
        let b = MoneyMicros::from_micros(2_500_000);
        assert_eq!(a.checked_add(b).unwrap().as_micros(), 3_500_000);
        assert_eq!(b.checked_sub(a).unwrap().as_micros(), 1_500_000);
        // A negative result is refused, not wrapped.
        assert_eq!(a.checked_sub(b), None);
        // Overflow is refused.
        assert_eq!(
            MoneyMicros::from_micros(u64::MAX).checked_add(MoneyMicros::from_micros(1)),
            None
        );
        // Display is the 6-decimal storage floor.
        assert_eq!(b.to_string(), "2.500000");
    }
}
