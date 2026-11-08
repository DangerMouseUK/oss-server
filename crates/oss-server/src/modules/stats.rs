//! `stats` and `basicstats`: per-player statistics and score updates (ASSS
//! `scoring/stats.c` and `scoring/basicstats.c`).
//!
//! Stats use ASSS's stat codes. Only the "reset" interval exists for now
//! (no persistent storage yet), and stats start from zero each time a
//! player enters an arena. The four public scores (kill points, flag points,
//! kills, deaths) are mirrored into [`crate::player::Score`] so they appear
//! in player records, and changed scores are broadcast as S2C `0x09` every
//! 5 seconds (ASSS sends them at persist interval ends and on entry).

use std::collections::HashMap;
use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::s2c::ScoreUpdate;

use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::core::{Action, PlayerAction};
use super::game::Kill;

/// ASSS stat codes (`statcodes.h`).
pub mod stat {
    /// Kill points.
    pub const KILL_POINTS: u16 = 0;
    /// Flag points.
    pub const FLAG_POINTS: u16 = 1;
    /// Kills.
    pub const KILLS: u16 = 2;
    /// Deaths.
    pub const DEATHS: u16 = 3;
    /// Assists.
    pub const ASSISTS: u16 = 100;
    /// Team kills.
    pub const TEAM_KILLS: u16 = 101;
    /// Deaths to a teammate.
    pub const TEAM_DEATHS: u16 = 102;
    /// Kills of flag carriers.
    pub const FLAG_KILLS: u16 = 204;
    /// Deaths while carrying flags.
    pub const FLAG_DEATHS: u16 = 205;
}

/// Statistics access (ASSS `Istats`).
pub trait Stats: Send + Sync {
    /// Adds `amount` to a stat.
    fn increment(&self, zone: &mut Zone, player: PlayerId, stat: u16, amount: i32);
    /// Sets a stat.
    fn set(&self, zone: &mut Zone, player: PlayerId, stat: u16, value: i32);
    /// Reads a stat (0 if never set).
    fn get(&self, zone: &Zone, player: PlayerId, stat: u16) -> i32;
    /// Broadcasts changed scores now (ASSS `SendUpdates`).
    fn send_updates(&self, zone: &mut Zone, exclude: Option<PlayerId>);
}

#[derive(Default)]
struct PlayerStats {
    values: HashMap<u16, i32>,
    dirty: bool,
}

fn mirror(zone: &mut Zone, player: PlayerId) {
    let Some(p) = zone.players.get_mut(player) else {
        return;
    };
    let get = |s: &PlayerStats, k| s.values.get(&k).copied().unwrap_or(0);
    let Some(s) = p.ext.get::<PlayerStats>() else {
        return;
    };
    let score = crate::player::Score {
        kill_points: get(s, stat::KILL_POINTS),
        flag_points: get(s, stat::FLAG_POINTS),
        wins: get(s, stat::KILLS) as i16,
        losses: get(s, stat::DEATHS) as i16,
    };
    p.score = score;
}

/// The stats implementation.
pub struct StatsImpl;

impl Stats for StatsImpl {
    fn increment(&self, zone: &mut Zone, player: PlayerId, stat: u16, amount: i32) {
        if let Some(p) = zone.players.get_mut(player) {
            let s = p.ext.get_or_default::<PlayerStats>();
            let v = s.values.entry(stat).or_default();
            *v = v.wrapping_add(amount);
            s.dirty = true;
        }
        mirror(zone, player);
    }

    fn set(&self, zone: &mut Zone, player: PlayerId, stat: u16, value: i32) {
        if let Some(p) = zone.players.get_mut(player) {
            let s = p.ext.get_or_default::<PlayerStats>();
            s.values.insert(stat, value);
            s.dirty = true;
        }
        mirror(zone, player);
    }

    fn get(&self, zone: &Zone, player: PlayerId, stat: u16) -> i32 {
        zone.players
            .get(player)
            .and_then(|p| p.ext.get::<PlayerStats>())
            .and_then(|s| s.values.get(&stat).copied())
            .unwrap_or(0)
    }

    fn send_updates(&self, zone: &mut Zone, exclude: Option<PlayerId>) {
        let mut updates = Vec::new();
        for p in zone.players.iter_mut() {
            if p.status != PlayerStatus::Playing || Some(p.id) == exclude {
                continue;
            }
            let Some(arena) = p.arena else { continue };
            let Some(s) = p.ext.get_mut::<PlayerStats>() else {
                continue;
            };
            if !std::mem::take(&mut s.dirty) {
                continue;
            }
            let sc = p.score;
            let pkt = ScoreUpdate {
                pid: p.id.0 as i16,
                kill_points: sc.kill_points,
                flag_points: sc.flag_points,
                kills: sc.wins,
                deaths: sc.losses,
            };
            updates.push((arena, pkt));
        }
        let mut buf = Vec::with_capacity(17);
        for (arena, pkt) in updates {
            buf.clear();
            pkt.encode(&mut buf);
            zone.send_to_arena(arena, exclude, &buf, SendFlags::UNRELIABLE);
        }
    }
}

/// The `stats` module.
pub struct StatsModule;

impl Module for StatsModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Stats>(me, Scope::Global, Arc::new(StatsImpl));
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if matches!(ev.action, Action::PreEnterArena | Action::LeaveArena)
                && let Some(p) = z.players.get_mut(ev.player)
            {
                p.ext.remove::<PlayerStats>();
                p.score = Default::default();
            }
        });
        zone.set_timer(me, 500, 500, |z| {
            StatsImpl.send_updates(z, None);
            true
        });
        Ok(())
    }
}

/// The `basicstats` module: counts kills, deaths, team kills and flag kills.
pub struct BasicStatsModule;

impl Module for BasicStatsModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on::<Kill>(me, Scope::Global, |z, k| {
            let Some(st) = z.iface::<dyn Stats>() else {
                return;
            };
            st.increment(z, k.killer, stat::KILLS, 1);
            st.increment(z, k.killed, stat::DEATHS, 1);
            let same =
                z.players.get(k.killer).map(|p| p.freq) == z.players.get(k.killed).map(|p| p.freq);
            if same {
                st.increment(z, k.killer, stat::TEAM_KILLS, 1);
                st.increment(z, k.killed, stat::TEAM_DEATHS, 1);
            }
            if k.flags > 0 {
                st.increment(z, k.killer, stat::FLAG_KILLS, 1);
                st.increment(z, k.killed, stat::FLAG_DEATHS, 1);
            }
        });
        Ok(())
    }
}

/// The `points_kill` module (attach to arenas): kill points from the killed
/// player's bounty, flag bonuses, none for team kills (ASSS
/// `scoring/points_kill.c`).
pub struct PointsKillModule;

impl Module for PointsKillModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(
        &self,
        zone: &mut Zone,
        me: Owner,
        arena: crate::arena::ArenaId,
    ) -> Result<(), ModuleError> {
        zone.on::<Kill>(me, Scope::Arena(arena), |z, k| {
            let Some(cfg) = z.arenas.get(k.arena).map(|a| a.cfg.clone()) else {
                return;
            };
            let (Some(killer), Some(killed)) = (z.players.get(k.killer), z.players.get(k.killed))
            else {
                return;
            };
            let team_kill = killer.freq == killed.freq;
            let fixed = cfg.get_int("Kill", "FixedKillReward", -1);
            let mut pts = if fixed != -1 {
                fixed
            } else {
                i32::from(k.bounty)
            };
            if i32::from(killer.pos.bounty) >= cfg.get_int("Kill", "FlagMinimumBounty", 0)
                && k.flags > 0
            {
                pts += i32::from(k.flags) * cfg.get_int("Kill", "PointsPerKilledFlag", 0);
            }
            if team_kill && cfg.get_int("Misc", "TeamKillPoints", 0) == 0 {
                pts = 0;
            }
            k.points.set(k.points.get() + pts);
        });
        Ok(())
    }
}
