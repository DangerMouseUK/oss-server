//! Tick-based timers (ASSS `mainloop` `SetTimer`), run on the game thread.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use crate::registry::Owner;
use crate::time::Ticks;
use crate::zone::Zone;

/// A timer callback. Return `true` to keep a repeating timer running.
pub type TimerFn = Arc<dyn Fn(&mut Zone) -> bool + Send + Sync>;

/// Handle to a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(u64);

struct Timer {
    owner: Owner,
    interval: u64,
    due: Ticks,
    f: TimerFn,
}

/// The timer wheel (a binary heap keyed by due tick).
#[derive(Default)]
pub struct Timers {
    heap: BinaryHeap<Reverse<(Ticks, u64)>>,
    timers: HashMap<u64, Timer>,
    next: u64,
}

impl Timers {
    /// Schedules `f` to run `delay` ticks from `now`, then every `interval`
    /// ticks while it returns `true` (an `interval` of 0 runs it once).
    pub fn add(
        &mut self,
        owner: Owner,
        now: Ticks,
        delay: u64,
        interval: u64,
        f: TimerFn,
    ) -> TimerId {
        let id = self.next;
        self.next += 1;
        let due = now + delay;
        self.timers.insert(
            id,
            Timer {
                owner,
                interval,
                due,
                f,
            },
        );
        self.heap.push(Reverse((due, id)));
        TimerId(id)
    }

    /// Cancels a timer.
    pub fn cancel(&mut self, id: TimerId) {
        self.timers.remove(&id.0);
    }

    /// Cancels every timer `owner` set.
    pub fn cancel_owner(&mut self, owner: Owner) {
        self.timers.retain(|_, t| t.owner != owner);
    }

    /// Number of live timers.
    pub fn len(&self) -> usize {
        self.timers.len()
    }

    /// True when no timers are set.
    pub fn is_empty(&self) -> bool {
        self.timers.is_empty()
    }

    fn pop_due(&mut self, now: Ticks) -> Option<(u64, TimerFn)> {
        while let Some(&Reverse((due, id))) = self.heap.peek() {
            if due > now {
                return None;
            }
            self.heap.pop();
            match self.timers.get(&id) {
                // stale heap entries (cancelled or rescheduled) are skipped
                Some(t) if t.due == due => return Some((id, t.f.clone())),
                _ => continue,
            }
        }
        None
    }

    fn reschedule(&mut self, id: u64, keep: bool) {
        let Some(t) = self.timers.get_mut(&id) else {
            return;
        };
        if keep && t.interval > 0 {
            t.due = t.due + t.interval;
            self.heap.push(Reverse((t.due, id)));
        } else {
            self.timers.remove(&id);
        }
    }
}

/// Runs every timer due at the zone's current tick.
pub fn run_due(zone: &mut Zone) {
    let now = zone.now();
    while let Some((id, f)) = zone.timers.pop_due(now) {
        let keep = f(zone);
        zone.timers.reschedule(id, keep);
    }
}
