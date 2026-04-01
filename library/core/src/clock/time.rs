use core::{
    fmt::Debug,
    ops::{Add, AddAssign, Div, Mul, MulAssign, Sub, SubAssign},
};

use chrono::{DateTime, NaiveDateTime};

use crate::clock::source::ClockSource;

const NANOS_PER_SEC: u64 = 1_000_000_000;
const NANOS_PER_MILLI: u64 = 1_000_000;
const NANOS_PER_MICRO: u64 = 1_000;
const MILLIS_PER_SEC: u64 = 1_000;
const MICROS_PER_SEC: u64 = 1_000_000;

#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct TimeStamp(Instant);

impl TimeStamp {
    pub fn new(instant: Instant) -> Self {
        Self(instant)
    }

    pub fn as_instant(&self) -> Instant {
        self.0
    }

    pub fn as_datetime(&self) -> NaiveDateTime {
        let secs = self.0.0.as_secs();
        let nanos = self.0.0.subsec_nanos();
        DateTime::from_timestamp(secs as i64, nanos)
            .unwrap()
            .naive_utc()
    }
}

impl From<NaiveDateTime> for TimeStamp {
    fn from(datetime: NaiveDateTime) -> Self {
        let secs = datetime.and_utc().timestamp();
        let nanos = datetime.and_utc().timestamp_subsec_nanos();
        Self(Instant(Duration::new(secs as u64, nanos)))
    }
}

/// Instant in time
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct Instant(pub(super) Duration);

impl Instant {
    /// Create a new instant
    pub fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Get the current instant in `Duration`
    pub fn as_duration(&self) -> Duration {
        self.0
    }

    /// Get the duration since another instant
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.0 - earlier.0
    }

    /// Get the duration until another instant
    pub fn elapsed(&self, source: &dyn ClockSource) -> Duration {
        Instant::now(source).duration_since(*self)
    }

    /// Get the current instant from a clock
    pub fn now(source: &dyn ClockSource) -> Self {
        source.now()
    }
}

impl core::ops::Add<Duration> for Instant {
    type Output = Self;

    fn add(self, other: Duration) -> Self {
        Instant(self.0 + other)
    }
}

impl core::ops::AddAssign<Duration> for Instant {
    fn add_assign(&mut self, other: Duration) {
        self.0 += other;
    }
}

impl core::ops::Sub<Duration> for Instant {
    type Output = Self;

    fn sub(self, other: Duration) -> Self {
        Instant(self.0 - other)
    }
}

impl core::ops::Sub for Instant {
    type Output = Duration;

    fn sub(self, other: Self) -> Duration {
        self.0 - other.0
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, Hash)]
pub struct Duration {
    secs: u64,
    nanos: u32,
}

impl Duration {
    const MAX: Duration = Duration {
        secs: u64::MAX,
        nanos: NANOS_PER_SEC as u32 - 1,
    };

    pub const fn new(secs: u64, nanos: u32) -> Self {
        let secs = secs + (nanos as u64 / NANOS_PER_SEC);
        let nanos = nanos % NANOS_PER_SEC as u32;
        Self { secs, nanos }
    }

    pub const fn from_secs(secs: u64) -> Self {
        Self { secs, nanos: 0 }
    }

    pub const fn from_millis(millis: u64) -> Self {
        Self {
            secs: millis / MILLIS_PER_SEC,
            nanos: ((millis % MILLIS_PER_SEC) * NANOS_PER_MILLI) as u32,
        }
    }

    pub const fn from_micros(micros: u64) -> Self {
        Self {
            secs: micros / MICROS_PER_SEC,
            nanos: ((micros % MICROS_PER_SEC) * NANOS_PER_MICRO) as u32,
        }
    }

    pub const fn from_nanos(nanos: u64) -> Self {
        Self {
            secs: nanos / NANOS_PER_SEC,
            nanos: (nanos % NANOS_PER_SEC) as u32,
        }
    }

    pub const fn as_secs(&self) -> u64 {
        self.secs
    }

    pub const fn as_millis(&self) -> u64 {
        self.secs * MILLIS_PER_SEC + (self.nanos as u64 / NANOS_PER_MILLI)
    }

    pub const fn as_micros(&self) -> u64 {
        self.secs * MICROS_PER_SEC + (self.nanos as u64 / NANOS_PER_MICRO)
    }

    pub const fn as_nanos(&self) -> u128 {
        self.secs as u128 * NANOS_PER_SEC as u128 + self.nanos as u128
    }

    pub const fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    pub fn checked_add(&self, other: Duration) -> Option<Duration> {
        let secs = self.secs.checked_add(other.secs)?;
        let nanos = self.nanos.checked_add(other.nanos)?;
        if nanos > NANOS_PER_SEC as u32 {
            Some(Duration {
                secs: secs.checked_add(1)?,
                nanos: nanos - NANOS_PER_SEC as u32,
            })
        } else {
            Some(Duration { secs, nanos })
        }
    }

    pub fn checked_sub(&self, other: Duration) -> Option<Duration> {
        let secs = self.secs.checked_sub(other.secs)?;
        let nanos = self.nanos as i64 - other.nanos as i64;
        if nanos < 0 {
            let abs_nanos = nanos.abs() as u64;
            Some(Duration {
                secs: secs.checked_sub(1)?,
                nanos: NANOS_PER_SEC as u32 - abs_nanos as u32,
            })
        } else {
            Some(Duration {
                secs,
                nanos: nanos as u32,
            })
        }
    }

    pub fn checked_mul(&self, other: u32) -> Option<Duration> {
        let secs = self.secs.checked_mul(other as u64)?;
        let nanos = self.nanos as u64 * other as u64;
        let secs = secs.checked_add(nanos / NANOS_PER_SEC)?;
        let nanos = (nanos % NANOS_PER_SEC) as u32;
        Some(Duration { secs, nanos })
    }

    /// TODO: Maybe incorrect
    pub fn checked_div(&self, other: u32) -> Option<Duration> {
        if other == 0 {
            return None;
        }
        let secs = self.secs.checked_div(other as u64)?;
        let nanos = (secs % other as u64 * NANOS_PER_SEC + self.nanos as u64) / other as u64;
        Some(Self::new(secs, nanos as u32))
    }

    pub fn saturating_add(&self, other: Duration) -> Duration {
        self.checked_add(other).unwrap_or_else(|| Duration::MAX)
    }

    pub fn saturating_sub(&self, other: Duration) -> Duration {
        self.checked_sub(other)
            .unwrap_or_else(|| Duration::from_secs(0))
    }

    pub fn saturating_mul(&self, other: u32) -> Duration {
        self.checked_mul(other).unwrap_or_else(|| Duration::MAX)
    }

    pub fn saturating_div(&self, other: u32) -> Duration {
        self.checked_div(other)
            .unwrap_or_else(|| Duration::from_secs(0))
    }

    pub fn wrapping_add(&self, other: Duration) -> Duration {
        Duration {
            secs: self.secs.wrapping_add(other.secs),
            nanos: self.nanos.wrapping_add(other.nanos),
        }
    }
}

impl PartialOrd for Duration {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        match self.checked_sub(*other) {
            Some(duration) if duration.secs == 0 && duration.nanos == 0 => {
                Some(core::cmp::Ordering::Equal)
            }
            Some(duration) if duration.secs > 0 || duration.nanos > 0 => {
                Some(core::cmp::Ordering::Greater)
            }
            None => Some(core::cmp::Ordering::Less),
            _ => None,
        }
    }
}

impl Debug for Duration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        enum Unit {
            Secs,
            Millis,
            Micros,
            Nanos,
        }
        let mut unit = Unit::Secs;
        if self.secs > 0 {
            write!(f, "{}", self.secs)?;
            if self.nanos > 0 {
                if self.nanos > NANOS_PER_MILLI as u32 {
                    write!(f, ".{:03}", self.nanos / NANOS_PER_MILLI as u32)?;
                } else if self.nanos > NANOS_PER_MICRO as u32 {
                    write!(f, ".{:06}", self.nanos / NANOS_PER_MICRO as u32)?;
                } else {
                    write!(f, ".{:09}", self.nanos)?;
                }
            }
        } else {
            if self.nanos > NANOS_PER_MILLI as u32 {
                write!(
                    f,
                    "{}.{:03}",
                    self.nanos / NANOS_PER_MILLI as u32,
                    self.nanos % NANOS_PER_MILLI as u32
                )?;
                unit = Unit::Millis;
            } else if self.nanos > NANOS_PER_MICRO as u32 {
                write!(
                    f,
                    "{}.{:06}",
                    self.nanos / NANOS_PER_MICRO as u32,
                    self.nanos % NANOS_PER_MICRO as u32
                )?;
                unit = Unit::Micros;
            } else {
                write!(f, "{}.{:09}", self.nanos, self.nanos)?;
                unit = Unit::Nanos;
            }
        }
        match unit {
            Unit::Secs => write!(f, "s"),
            Unit::Millis => write!(f, "ms"),
            Unit::Micros => write!(f, "us"),
            Unit::Nanos => write!(f, "ns"),
        }
    }
}

impl Add<Duration> for Duration {
    type Output = Self;

    fn add(self, other: Duration) -> Self {
        let mut secs = self.secs + other.secs;
        let mut nanos = self.nanos + other.nanos;
        if nanos >= NANOS_PER_SEC as u32 {
            secs += 1;
            nanos -= NANOS_PER_SEC as u32;
        }
        Self { secs, nanos }
    }
}

impl AddAssign<Duration> for Duration {
    fn add_assign(&mut self, other: Duration) {
        *self = *self + other;
    }
}

impl Sub<Duration> for Duration {
    type Output = Self;

    fn sub(self, other: Duration) -> Self {
        let mut secs = self.secs;
        let mut nanos = self.nanos as i64 - other.nanos as i64;
        if nanos < 0 {
            let abs_nanos = nanos.abs() as u64;
            secs -= 1;
            nanos = NANOS_PER_SEC as i64 - abs_nanos as i64;
        }
        secs -= other.secs;
        Self {
            secs,
            nanos: nanos as u32,
        }
    }
}

impl SubAssign<Duration> for Duration {
    fn sub_assign(&mut self, other: Duration) {
        *self = *self - other;
    }
}

impl Mul<u32> for Duration {
    type Output = Self;

    fn mul(self, other: u32) -> Self {
        let secs = self.secs * other as u64;
        let nanos = self.nanos as u64 * other as u64;
        let secs = secs + nanos / NANOS_PER_SEC;
        let nanos = (nanos % NANOS_PER_SEC) as u32;
        Self { secs, nanos }
    }
}

impl MulAssign<u32> for Duration {
    fn mul_assign(&mut self, other: u32) {
        *self = *self * other;
    }
}

impl Div<u32> for Duration {
    type Output = Self;

    /// TODO: Maybe incorrect
    fn div(self, other: u32) -> Self {
        let secs = self.secs / (other as u64);
        let nanos = (secs % other as u64 * NANOS_PER_SEC + self.nanos as u64) / other as u64;
        Self::new(secs, nanos as u32)
    }
}
