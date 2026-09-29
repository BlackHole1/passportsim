//! Virtual time. Every conversion saturates instead of overflowing, so debug and release builds
//! give the same result.

use serde::{Deserialize, Serialize};

pub(crate) const PS_PER_S: u64 = 1_000_000_000_000;
pub(crate) const PS_PER_US: u64 = 1_000_000;

/// Virtual time in picoseconds since the first power-on (2^64 ps is about 213 days).
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub struct VTime(pub u64);

impl VTime {
    pub const fn from_us(us: u64) -> Self {
        VTime(us.saturating_mul(PS_PER_US))
    }

    pub const fn from_ms(ms: u64) -> Self {
        VTime(ms.saturating_mul(PS_PER_US * 1_000))
    }

    /// Whole microseconds, rounded down.
    pub const fn as_us(self) -> u64 {
        self.0 / PS_PER_US
    }
}

/// Exact audio clock: frame n at fs starts at start + n * 1e12 / fs, computed in u128 so nothing
/// accumulates drift. The division floors (UNVERIFIED: the rounding is a choice no source fixes).
/// Saturates at `u64::MAX` ps; `fs_hz` 0 (an unconfigured I2S clock) returns `start`.
pub fn frame_time(start: VTime, n: u64, fs_hz: u32) -> VTime {
    if fs_hz == 0 {
        return start;
    }
    let offset = (n as u128 * PS_PER_S as u128) / fs_hz as u128;
    let t = start.0 as u128 + offset;
    VTime(if t > u64::MAX as u128 {
        u64::MAX
    } else {
        t as u64
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert_eq!(VTime::from_us(3), VTime(3_000_000));
        assert_eq!(VTime::from_ms(2), VTime(2_000_000_000));
        assert_eq!(VTime(1_999_999).as_us(), 1);
        assert_eq!(VTime(999_999).as_us(), 0);
        assert_eq!(VTime::from_us(42).as_us(), 42);
        assert_eq!(VTime::from_ms(5).as_us(), 5_000);
        assert_eq!(VTime::default(), VTime(0));
    }

    #[test]
    fn frame_time_is_exact_without_drift() {
        let start = VTime(5);
        assert_eq!(frame_time(start, 0, 44_100), start);
        assert_eq!(
            frame_time(start, 44_100, 44_100),
            VTime(5 + 1_000_000_000_000)
        );
        // 1e12 / 44100 = 22675736.96..., floored per frame index, not accumulated.
        assert_eq!(frame_time(VTime(0), 1, 44_100), VTime(22_675_736));
        assert_eq!(frame_time(VTime(0), 3, 44_100), VTime(68_027_210));
        let n = 16_000u64 * 86_400 * 100;
        assert_eq!(
            frame_time(VTime(0), n, 16_000),
            VTime(86_400 * 100 * 1_000_000_000_000)
        );
    }

    #[test]
    fn conversions_and_frame_time_saturate() {
        assert_eq!(VTime::from_us(u64::MAX), VTime(u64::MAX));
        assert_eq!(VTime::from_ms(u64::MAX / 1_000), VTime(u64::MAX));
        assert_eq!(frame_time(VTime(u64::MAX - 1), 1, 1), VTime(u64::MAX));
        assert_eq!(frame_time(VTime(0), u64::MAX, 1), VTime(u64::MAX));
        assert_eq!(frame_time(VTime(7), 123, 0), VTime(7));
    }

    #[test]
    fn integer_periods_of_agent_2_1() {
        // Every clock period the machine uses is a whole number of picoseconds.
        assert_eq!(frame_time(VTime(0), 1, 16_000), VTime(62_500_000));
        assert_eq!(PS_PER_S / 160_000_000, 6_250);
        assert_eq!(PS_PER_S / 80_000_000, 12_500);
        assert_eq!(PS_PER_S / 40_000_000, 25_000);
        assert_eq!(VTime::from_us(1), VTime(1_000_000));
        assert_eq!(VTime::from_ms(1), VTime(1_000_000_000));
        assert_eq!(VTime::from_us(1_000_000), VTime(PS_PER_S));
    }
}
