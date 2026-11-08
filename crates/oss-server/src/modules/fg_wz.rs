//! `fg_wz`: the warzone flag game (attach to arenas; ASSS `fg_wz.py`).
//!
//! `Flag:FlagCount` flags (a number or a `min-max` range, picked per game)
//! spawn around `Flag:SpawnX/SpawnY` within `Flag:SpawnRadius`. Players carry
//! them (`Flag:CarryFlags`); dropped flags land near the carrier (or in the
//! center) and are owned by the carrier's team or neutral depending on how
//! they were dropped (`Drop*`, `Neut*`, `TK*`, `Safe*` settings). When every
//! flag on the map belongs to one team for `Flag:WinDelay` ticks, that team
//! wins: [`WarzoneWin`] handlers add points (1000 if none do) and the game
//! resets. Not yet ported: no-flag regions.

use std::cell::Cell;
use std::sync::Arc;

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::prng::Prng;
use crate::registry::{Event, Owner, Scope};
use crate::timers::TimerId;
use crate::zone::Zone;

use super::flagcore::{
    self, CARRY_ALL, Cleanup, FlagGame, FlagInfo, FlagOnMap, FlagState, MAX_FLAGS,
};

/// ASSS `CB_WARZONEWIN`: a team won; handlers add to `points`.
#[derive(Debug)]
pub struct WarzoneWin {
    /// Arena.
    pub arena: ArenaId,
    /// Winning frequency.
    pub freq: i16,
    /// Points to award.
    pub points: Cell<i32>,
}

impl Event for WarzoneWin {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

#[derive(Clone)]
struct Settings {
    spawn_x: i32,
    spawn_y: i32,
    spawn_r: i32,
    drop_r: i32,
    friendly_transfer: bool,
    carry_flags: i32,
    drop_owned: bool,
    drop_center: bool,
    neut_owned: bool,
    neut_center: bool,
    tk_owned: bool,
    tk_center: bool,
    safe_owned: bool,
    safe_center: bool,
    win_delay: u64,
    min: usize,
    max: usize,
}

impl Settings {
    fn load(cfg: &crate::config::ConfigFile) -> Self {
        let b = |k: &str, d: i32| cfg.get_int("Flag", k, d) != 0;
        let count = cfg.get("Flag", "FlagCount").unwrap_or("0");
        let (min, max) = match count.split_once('-') {
            Some((a, b)) => (a.trim().parse().unwrap_or(0), b.trim().parse().unwrap_or(0)),
            None => {
                let n = count.trim().parse().unwrap_or(0);
                (n, n)
            }
        };
        let max: usize = max.min(MAX_FLAGS);
        Self {
            spawn_x: cfg.get_int("Flag", "SpawnX", 512),
            spawn_y: cfg.get_int("Flag", "SpawnY", 512),
            spawn_r: cfg.get_int("Flag", "SpawnRadius", 50),
            drop_r: cfg.get_int("Flag", "DropRadius", 2),
            friendly_transfer: b("FriendlyTransfer", 1),
            carry_flags: cfg.get_int("Flag", "CarryFlags", CARRY_ALL),
            drop_owned: b("DropOwned", 1),
            drop_center: b("DropCenter", 0),
            neut_owned: b("NeutOwned", 0),
            neut_center: b("NeutCenter", 0),
            tk_owned: b("TKOwned", 1),
            tk_center: b("TKCenter", 0),
            safe_owned: b("SafeOwned", 1),
            safe_center: b("SafeCenter", 0),
            win_delay: u64::try_from(cfg.get_int("Flag", "WinDelay", 200)).unwrap_or(200),
            min: min.min(max),
            max,
        }
    }
}

struct Wz {
    sets: Settings,
    current: usize,
    neuts: Vec<usize>,
    win_timer: Option<TimerId>,
}

fn wz(zone: &mut Zone, a: ArenaId) -> Option<&mut Wz> {
    zone.arenas.get_mut(a)?.ext.get_mut::<Wz>()
}

/// Reflects `v` back into `lo..=hi` (ASSS `wrap`).
fn wrap(mut v: i32, lo: i32, hi: i32) -> i32 {
    for _ in 0..8 {
        if v < lo {
            v = 2 * lo - v;
        } else if v > hi {
            v = 2 * hi - v;
        } else {
            return v;
        }
    }
    v.clamp(lo, hi)
}

fn circular_random(rng: &mut Prng, cx: i32, cy: i32, radius: i32) -> (i32, i32) {
    let uniform = |rng: &mut Prng| f64::from(rng.next_u32()) / 4_294_967_296.0;
    let r = uniform(rng) * f64::from(radius);
    let theta = uniform(rng) * std::f64::consts::TAU;
    let x = (f64::from(cx) + 0.5 + r * theta.cos()) as i32;
    let y = (f64::from(cy) + 0.5 + r * theta.sin()) as i32;
    (wrap(x, 0, 1023), wrap(y, 0, 1023))
}

/// Places flag `fid` near `(cx, cy)`, avoiding walls and other flags, falling
/// back to the spawn center (ASSS `spawn_flag`).
#[allow(clippy::too_many_arguments)]
fn spawn_flag(
    zone: &mut Zone,
    a: ArenaId,
    fid: usize,
    cx: i32,
    cy: i32,
    r: i32,
    freq: i16,
    fallback: bool,
) {
    let current = wz(zone, a).map_or(0, |w| w.current);
    let map = super::mapdata::arena_map(zone, a).cloned();
    let mut placed = None;
    for tries in 0..30 {
        let mut rng = std::mem::take(zone.res.get_or_default::<Prng>());
        let (mut x, mut y) = circular_random(&mut rng, cx, cy, r + tries);
        *zone.res.get_or_default::<Prng>() = rng;
        if let Some(m) = &map {
            (x, y) = m.find_empty_tile_near(x, y);
        }
        let clash = (0..current).any(|j| {
            j != fid
                && flagcore::get_flag(zone, a, j).is_some_and(|f| {
                    f.state == FlagState::OnMap && i32::from(f.x) == x && i32::from(f.y) == y
                })
        });
        if !clash {
            placed = Some((x, y));
            break;
        }
    }
    match placed {
        Some((x, y)) => {
            flagcore::set_flag(zone, a, fid, FlagInfo::on_map(x as i16, y as i16, freq))
        }
        None if fallback => {
            tracing::warn!(target: "fg_wz", arena = a.0, cx, cy, r, "failed to place a flag, falling back to center");
            if let Some(s) = wz(zone, a).map(|w| w.sets.clone()) {
                spawn_flag(zone, a, fid, s.spawn_x, s.spawn_y, s.spawn_r, freq, false);
            }
        }
        None => {
            tracing::error!(target: "fg_wz", arena = a.0, "failed to place a flag, fallback disabled")
        }
    }
}

fn neut_flag(zone: &mut Zone, a: ArenaId, fid: usize, x: i32, y: i32, freq: i16) {
    if let Some(w) = wz(zone, a) {
        w.neuts.push(fid);
    }
    flagcore::set_flag(
        zone,
        a,
        fid,
        FlagInfo {
            state: FlagState::None,
            carrier: None,
            x: x as i16,
            y: y as i16,
            freq,
        },
    );
}

fn spawn_timer(zone: &mut Zone, a: ArenaId) -> bool {
    let Some(w) = wz(zone, a) else { return false };
    if w.current < w.sets.min {
        let (lo, hi) = (w.sets.min, w.sets.max);
        let n = lo
            + zone
                .res
                .get_or_default::<Prng>()
                .below((hi - lo + 1) as u32) as usize;
        if let Some(w) = wz(zone, a) {
            w.current = n;
        }
    }
    let Some(w) = wz(zone, a) else { return false };
    let (current, sets) = (w.current, w.sets.clone());
    for fid in 0..current {
        let Some(f) = flagcore::get_flag(zone, a, fid) else {
            continue;
        };
        if f.state != FlagState::None {
            continue;
        }
        let was_neut = wz(zone, a).is_some_and(|w| {
            let i = w.neuts.iter().position(|&n| n == fid);
            if let Some(i) = i {
                w.neuts.remove(i);
            }
            i.is_some()
        });
        if was_neut {
            let r = if sets.neut_center {
                sets.spawn_r
            } else {
                sets.drop_r
            };
            spawn_flag(
                zone,
                a,
                fid,
                i32::from(f.x),
                i32::from(f.y),
                r,
                f.freq,
                true,
            );
        } else {
            spawn_flag(
                zone,
                a,
                fid,
                sets.spawn_x,
                sets.spawn_y,
                sets.spawn_r,
                -1,
                true,
            );
        }
    }
    true
}

fn check_win(zone: &mut Zone, a: ArenaId) {
    let Some(current) = wz(zone, a).map(|w| w.current) else {
        return;
    };
    let mut owned = 0;
    let mut win_freq = -1i16;
    for fid in 0..current {
        let Some(f) = flagcore::get_flag(zone, a, fid) else {
            continue;
        };
        if f.state == FlagState::OnMap {
            if f.freq == win_freq {
                owned += 1;
            } else {
                win_freq = f.freq;
                owned = 1;
            }
        }
    }
    if current > 0 && owned == current && win_freq != -1 {
        let ev = WarzoneWin {
            arena: a,
            freq: win_freq,
            points: Cell::new(0),
        };
        zone.fire(&ev);
        let points = match ev.points.get() {
            0 => 1000,
            p => p,
        };
        flagcore::flag_reset(zone, a, win_freq, points);
        if let Some(w) = wz(zone, a) {
            w.current = 0;
        }
    }
}

/// The warzone game.
pub struct Warzone {
    me: Owner,
}

impl FlagGame for Warzone {
    fn init(&self, zone: &mut Zone, arena: ArenaId) {
        let Some(cfg) = zone.arenas.get(arena).map(|a| a.cfg.clone()) else {
            return;
        };
        let sets = Settings::load(&cfg);
        if sets.carry_flags < CARRY_ALL {
            tracing::error!(target: "fg_wz", arena = arena.0, "invalid Flag:CarryFlags for warzone-style game");
            return;
        }
        flagcore::set_carry_mode(zone, arena, sets.carry_flags);
        flagcore::reserve_flags(zone, arena, sets.max);
        let me = self.me;
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.insert(Wz {
                sets,
                current: 0,
                neuts: Vec::new(),
                win_timer: None,
            });
        }
        zone.set_timer(me, 500, 500, move |z| spawn_timer(z, arena));
    }

    fn flag_touch(&self, zone: &mut Zone, arena: ArenaId, player: PlayerId, fid: usize) {
        let Some(carry) = wz(zone, arena).map(|w| w.sets.carry_flags) else {
            return;
        };
        let can_carry = if carry == CARRY_ALL {
            MAX_FLAGS as i16
        } else {
            (carry - 1) as i16
        };
        if zone
            .players
            .get(player)
            .is_some_and(|p| p.flags_carried >= can_carry)
        {
            tracing::warn!(target: "fg_wz", pid = player.0, "tried to pick up too many flags");
            return;
        }
        flagcore::set_flag(zone, arena, fid, FlagInfo::carried(player));
    }

    fn cleanup(
        &self,
        zone: &mut Zone,
        arena: ArenaId,
        fid: usize,
        reason: Cleanup,
        carrier: PlayerId,
        freq: i16,
    ) {
        let Some(s) = wz(zone, arena).map(|w| w.sets.clone()) else {
            return;
        };
        let (px, py) = zone
            .players
            .get(carrier)
            .map_or((s.spawn_x, s.spawn_y), |p| {
                (i32::from(p.pos.x) >> 4, i32::from(p.pos.y) >> 4)
            });
        let at = |center: bool| {
            if center {
                (s.spawn_x, s.spawn_y, s.spawn_r)
            } else {
                (px, py, s.drop_r)
            }
        };
        let owner = |owned: bool| if owned { freq } else { -1 };
        match reason {
            Cleanup::Dropped | Cleanup::KillCantCarry | Cleanup::KillFake => {
                let (x, y, r) = at(s.drop_center);
                spawn_flag(zone, arena, fid, x, y, r, owner(s.drop_owned), true);
            }
            Cleanup::InSafe => {
                let (x, y, r) = at(s.safe_center);
                spawn_flag(zone, arena, fid, x, y, r, owner(s.safe_owned), true);
            }
            Cleanup::KillTk => {
                if !s.friendly_transfer {
                    let (x, y, r) = at(s.tk_center);
                    spawn_flag(zone, arena, fid, x, y, r, owner(s.tk_owned), true);
                }
            }
            Cleanup::ShipChange | Cleanup::FreqChange | Cleanup::LeftArena | Cleanup::Other => {
                let (x, y, _) = at(s.neut_center);
                neut_flag(zone, arena, fid, x, y, owner(s.neut_owned));
            }
            Cleanup::KillNormal => {}
        }
    }
}

/// The `fg_wz` module (attach to arenas).
pub struct FgWzModule;

impl Module for FgWzModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn FlagGame>(me, Scope::Arena(arena), Arc::new(Warzone { me }));
        zone.on::<FlagOnMap>(me, Scope::Arena(arena), move |z, ev| {
            // any flag landing restarts the win countdown
            let Some(delay) = wz(z, ev.arena).map(|w| w.sets.win_delay) else {
                return;
            };
            if let Some(old) = wz(z, ev.arena).and_then(|w| w.win_timer.take()) {
                z.timers.cancel(old);
            }
            let a = ev.arena;
            let t = z.set_timer(me, delay, 0, move |z| {
                if let Some(w) = wz(z, a) {
                    w.win_timer = None;
                }
                check_win(z, a);
                false
            });
            if let Some(w) = wz(z, ev.arena) {
                w.win_timer = Some(t);
            }
        });
        Ok(())
    }

    fn detach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) {
        let _ = me;
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.remove::<Wz>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_reflects() {
        assert_eq!(wrap(-3, 0, 1023), 3);
        assert_eq!(wrap(1030, 0, 1023), 1016);
        assert_eq!(wrap(500, 0, 1023), 500);
    }

    #[test]
    fn flag_count_ranges() {
        let s = Settings::load(&crate::config::ConfigFile::from_text(
            "[Flag]\nFlagCount = 3-7\n",
        ));
        assert_eq!((s.min, s.max), (3, 7));
        let s = Settings::load(&crate::config::ConfigFile::from_text(
            "[Flag]\nFlagCount = 999\n",
        ));
        assert_eq!((s.min, s.max), (255, 255));
    }
}
