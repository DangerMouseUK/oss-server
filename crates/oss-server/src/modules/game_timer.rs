//! `game_timer`: the arena's game clock (ASSS `game_timer.c`).
//!
//! An arena with `Misc:TimedGame` set runs a game of that many ticks,
//! starting when the arena is created and starting again each time it runs
//! out. An arena without it has no clock until staff set one with `?timer`,
//! which can also be paused and resumed.
//!
//! Commands: `?time` says how long is left, `?timer <minutes>[:<seconds>]`
//! sets one (only where `Misc:TimedGame` is off), `?timereset` restarts a
//! timed game, and `?pausetimer` stops and starts a timer set by hand.
//!
//! `Misc:TimerWarnings` is a list of up to five marks, in seconds; the arena
//! is told when the clock passes each one. When the clock runs out everyone
//! hears "Game over" and [`TimesUp`] is raised, which is the hook a game
//! module uses to end a round.

use crate::arena::ArenaId;
use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::registry::{Event, Owner, Scope};
use crate::time::{TICKS_PER_SECOND, Ticks};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};
use super::chat::{Chat, message};

/// ASSS `CB_TIMESUP`: an arena's game timer ran out.
#[derive(Debug, Clone, Copy)]
pub struct TimesUp {
    /// The arena whose timer expired.
    pub arena: ArenaId,
}

impl Event for TimesUp {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS's `SOUND_HALLELLULA`.
const SOUND_GAME_OVER: u8 = 5;

/// How many warning marks ASSS reads.
const MAX_WARNINGS: usize = 5;

/// An arena's clock. `timeout` means two things, as it does in ASSS: while
/// the timer runs it is the tick the game ends on; while it is paused it is
/// how many ticks are left. Zero with `enabled` false means there is no
/// timer at all.
#[derive(Debug, Default, Clone)]
struct TimerData {
    /// `Misc:TimedGame` in ticks; 0 when the arena has no fixed game length.
    game_len: u64,
    enabled: bool,
    timeout: u64,
    /// `Misc:TimerWarnings`, in seconds.
    warn: Vec<u64>,
}

fn td(zone: &mut Zone, arena: ArenaId) -> Option<&mut TimerData> {
    zone.arenas
        .get_mut(arena)
        .map(|a| a.ext.get_or_default::<TimerData>())
}

fn read(zone: &Zone, arena: ArenaId) -> TimerData {
    zone.arenas
        .get(arena)
        .and_then(|a| a.ext.get::<TimerData>())
        .cloned()
        .unwrap_or_default()
}

fn say_arena(zone: &mut Zone, arena: ArenaId, sound: u8, text: &str) {
    let to: Vec<crate::player::PlayerId> = zone.players.in_arena(arena).map(|p| p.id).collect();
    if let Some(c) = zone.iface::<dyn Chat>() {
        c.send_any_message(
            zone,
            &to,
            oss_proto::common::chat_type::ARENA,
            sound,
            -1,
            text,
        );
    }
}

/// Ticks left, 0 when there is no timer.
fn remaining(data: &TimerData, now: Ticks) -> u64 {
    if data.enabled {
        data.timeout.saturating_sub(now.0)
    } else {
        data.timeout
    }
}

fn on_conf(zone: &mut Zone, arena: ArenaId, created: bool) {
    let (warn, game_len) = {
        let Some(a) = zone.arenas.get(arena) else {
            return;
        };
        let warn: Vec<u64> = a
            .cfg
            .get("Misc", "TimerWarnings")
            .unwrap_or("")
            .split([' ', ','])
            .filter(|w| !w.is_empty())
            .filter_map(|w| w.parse::<u64>().ok())
            .take(MAX_WARNINGS)
            .collect();
        let len = a.cfg.get_int("Misc", "TimedGame", 0).max(0) as u64;
        (warn, len)
    };
    let now = zone.now();
    let Some(d) = td(zone, arena) else { return };
    let was = d.game_len;
    d.warn = warn;
    d.game_len = game_len;
    // a new arena with a timed game starts its clock; so does one that has
    // just been given `Misc:TimedGame` by a config change
    if game_len > 0 && (created || was == 0) {
        d.enabled = true;
        d.timeout = now.0 + game_len;
    }
}

/// Runs once a second: expires timers and sends the warnings.
fn tick(zone: &mut Zone) -> bool {
    let now = zone.now();
    for arena in zone.arenas.ids() {
        let data = read(zone, arena);
        if !data.enabled {
            continue;
        }
        if now.0 >= data.timeout {
            tracing::debug!(target: "game_timer", arena = arena.0, "timer expired");
            zone.fire(&TimesUp { arena });
            say_arena(zone, arena, SOUND_GAME_OVER, "NOTICE: Game over");
            if let Some(d) = td(zone, arena) {
                if d.game_len > 0 {
                    // a timed game starts the next one at once
                    d.timeout = now.0 + d.game_len;
                } else {
                    d.enabled = false;
                    d.timeout = 0;
                }
            }
            continue;
        }
        let left = (data.timeout - now.0) / TICKS_PER_SECOND;
        if left > 0 && data.warn.contains(&left) {
            let text = if left.is_multiple_of(60) {
                let mins = left / 60;
                let s = if left == 60 { "" } else { "s" };
                format!("NOTICE: {mins} minute{s} remaining.")
            } else {
                format!("NOTICE: {left} seconds remaining.")
            };
            say_arena(zone, arena, 0, &text);
        }
    }
    true
}

fn c_time(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let data = read(zone, arena);
    let now = zone.now();
    let left = remaining(&data, now);
    let (mins, secs) = (left / TICKS_PER_SECOND / 60, (left / TICKS_PER_SECOND) % 60);
    let text = if data.enabled {
        format!("Time left: {mins} minutes {secs} seconds")
    } else if data.timeout > 0 {
        format!("Timer paused at:  {mins} minutes {secs} seconds")
    } else {
        "Time left: 0 minutes 0 seconds".to_owned()
    };
    message(zone, inv.sender, &text);
}

fn c_timer(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    if read(zone, arena).game_len > 0 {
        message(
            zone,
            inv.sender,
            "Timer is fixed to Misc:TimedGame setting.",
        );
        return;
    }
    let params = inv.params.trim();
    let (mins, secs) = match params.split_once(':') {
        Some((m, s)) => (
            m.trim().parse::<u64>().ok(),
            s.trim().parse::<u64>().unwrap_or(0),
        ),
        None => (params.parse::<u64>().ok(), 0),
    };
    let Some(mins) = mins else {
        message(zone, inv.sender, "timer format is: '?timer mins[:secs]'");
        return;
    };
    let now = zone.now();
    if let Some(d) = td(zone, arena) {
        d.enabled = true;
        d.timeout = now.0 + (mins * 60 + secs) * TICKS_PER_SECOND;
    }
    c_time(zone, inv);
}

fn c_timereset(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let now = zone.now();
    let reset = td(zone, arena).is_some_and(|d| {
        if d.game_len == 0 {
            return false;
        }
        d.enabled = true;
        d.timeout = now.0 + d.game_len;
        true
    });
    if reset {
        c_time(zone, inv);
    }
}

fn c_pausetimer(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let now = zone.now();
    let data = read(zone, arena);
    if data.game_len > 0 {
        return;
    }
    let (text, left) = if data.enabled {
        let left = data.timeout.saturating_sub(now.0);
        ("Timer paused at: ", left)
    } else if data.timeout > 0 {
        ("Timer resumed at:", data.timeout)
    } else {
        return;
    };
    if let Some(d) = td(zone, arena) {
        if d.enabled {
            d.enabled = false;
            d.timeout = left;
        } else {
            d.enabled = true;
            d.timeout = now.0 + left;
        }
    }
    let (mins, secs) = (left / TICKS_PER_SECOND / 60, (left / TICKS_PER_SECOND) % 60);
    message(
        zone,
        inv.sender,
        &format!("{text} {mins} minutes {secs} seconds"),
    );
}

/// The `game_timer` module.
pub struct GameTimerModule;

impl Module for GameTimerModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create => on_conf(z, ev.arena, true),
            ArenaActionKind::ConfChanged => on_conf(z, ev.arena, false),
            ArenaActionKind::Destroy => {
                if let Some(d) = td(z, ev.arena) {
                    d.enabled = false;
                    d.timeout = 0;
                }
            }
            _ => {}
        });
        zone.set_timer(me, TICKS_PER_SECOND, TICKS_PER_SECOND, tick);
        let help = |args: &str, what: &str| {
            format!("Module: game_timer\nTargets: none\nArgs: {args}\n{what}\n")
        };
        zone.add_command(
            me,
            "time",
            None,
            Some(&help("none", "Says how long is left in this arena's game.")),
            c_time,
        );
        zone.add_command(
            me,
            "timer",
            None,
            Some(&help(
                "<minutes>[:<seconds>]",
                "Sets the arena timer, in arenas where Misc:TimedGame is off.",
            )),
            c_timer,
        );
        zone.add_command(
            me,
            "timereset",
            None,
            Some(&help(
                "none",
                "Starts a timed game again, in arenas using Misc:TimedGame.",
            )),
            c_timereset,
        );
        zone.add_command(
            me,
            "pausetimer",
            None,
            Some(&help(
                "none",
                "Pauses or resumes a timer that was set with ?timer.",
            )),
            c_pausetimer,
        );
        Ok(())
    }
}
