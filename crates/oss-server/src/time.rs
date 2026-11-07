//! Server time in SubSpace ticks (hundredths of a second).

use std::ops::{Add, Sub};
use std::time::Duration;

/// Ticks per second: SubSpace's native timebase.
pub const TICKS_PER_SECOND: u64 = 100;

/// A point in server time, in ticks since the server started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ticks(pub u64);

impl Ticks {
    /// The 32-bit wrapping timestamp used on the wire.
    pub fn wire(self) -> u32 {
        self.0 as u32
    }

    /// Ticks elapsed since `earlier` (zero if `earlier` is later).
    pub fn since(self, earlier: Ticks) -> u64 {
        self.0.saturating_sub(earlier.0)
    }

    /// Converts a duration to whole ticks.
    pub fn from_duration(d: Duration) -> Ticks {
        Ticks((d.as_millis() / 10) as u64)
    }

    /// Converts to a duration.
    pub fn to_duration(self) -> Duration {
        Duration::from_millis(self.0.saturating_mul(10))
    }
}

impl Add<u64> for Ticks {
    type Output = Ticks;
    fn add(self, rhs: u64) -> Ticks {
        Ticks(self.0.saturating_add(rhs))
    }
}

impl Sub<u64> for Ticks {
    type Output = Ticks;
    fn sub(self, rhs: u64) -> Ticks {
        Ticks(self.0.saturating_sub(rhs))
    }
}
