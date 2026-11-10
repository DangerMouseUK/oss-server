//! `stats` and `basicstats`: per-player statistics and score updates (ASSS
//! `scoring/stats.c` and `scoring/basicstats.c`).
//!
//! Stats use ASSS's stat codes. The forever, reset and game intervals are
//! registered as [`super::persist`] slots, so a player's stats come back
//! with them when they log in; [`super::persist::end_interval`] starts a new
//! period. The four public scores (kill points, flag points,
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
use super::persist::{Interval, PersistScope, PlayerData, register_player_data};

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
    /// Flags picked up.
    pub const FLAG_PICKUPS: u16 = 200;
    /// Flags dropped.
    pub const FLAG_DROPS: u16 = 202;
    /// Flags neuted (ship/freq change, leaving).
    pub const FLAG_NEUT_DROPS: u16 = 203;
    /// Flag games won.
    pub const FLAG_GAMES_WON: u16 = 206;
    /// Flag games lost.
    pub const FLAG_GAMES_LOST: u16 = 207;
    /// Turf flags claimed.
    pub const TURF_TAGS: u16 = 208;
    /// Ball pickups.
    pub const BALL_CARRIES: u16 = 300;
    /// Goals scored.
    pub const BALL_GOALS: u16 = 302;
    /// Ball games won.
    pub const BALL_GAMES_WON: u16 = 303;
    /// Ball games lost.
    pub const BALL_GAMES_LOST: u16 = 304;
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
    /// the "reset" interval: what scores show
    values: HashMap<u16, i32>,
    forever: HashMap<u16, i32>,
    game: HashMap<u16, i32>,
    dirty: bool,
}

impl PlayerStats {
    fn interval(&mut self, i: Interval) -> &mut HashMap<u16, i32> {
        match i {
            Interval::Forever | Interval::ForeverNonshared => &mut self.forever,
            Interval::Game => &mut self.game,
            Interval::Reset | Interval::MapRotation => &mut self.values,
        }
    }
}

fn encode_stats(m: &HashMap<u16, i32>) -> Vec<u8> {
    let mut v: Vec<(&u16, &i32)> = m.iter().collect();
    v.sort();
    let mut out = Vec::with_capacity(6 * v.len());
    for (k, val) in v {
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out
}

fn decode_stats(b: &[u8]) -> HashMap<u16, i32> {
    b.as_chunks::<6>()
        .0
        .iter()
        .map(|c| {
            (
                u16::from_le_bytes([c[0], c[1]]),
                i32::from_le_bytes([c[2], c[3], c[4], c[5]]),
            )
        })
        .collect()
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
            for m in [&mut s.values, &mut s.forever, &mut s.game] {
                let v = m.entry(stat).or_default();
                *v = v.wrapping_add(amount);
            }
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
            // cleared on entry; persist (if loaded) then restores saved stats
            if ev.action == Action::PreEnterArena
                && let Some(p) = z.players.get_mut(ev.player)
            {
                p.ext.remove::<PlayerStats>();
                p.score = Default::default();
            }
        });
        for interval in [Interval::Forever, Interval::Reset, Interval::Game] {
            register_player_data(
                zone,
                me,
                PlayerData {
                    key: 1,
                    interval,
                    scope: PersistScope::PerArena,
                    get: Arc::new(move |z, p, _| {
                        let s = z.players.get(p)?.ext.get::<PlayerStats>()?;
                        let m = match interval {
                            Interval::Forever => &s.forever,
                            Interval::Game => &s.game,
                            _ => &s.values,
                        };
                        (!m.is_empty()).then(|| encode_stats(m))
                    }),
                    set: Arc::new(move |z, p, _, b| {
                        if let Some(pl) = z.players.get_mut(p) {
                            let s = pl.ext.get_or_default::<PlayerStats>();
                            *s.interval(interval) = decode_stats(b);
                            s.dirty = true;
                        }
                        mirror(z, p);
                    }),
                    clear: Arc::new(move |z, p, _| {
                        if let Some(pl) = z.players.get_mut(p) {
                            pl.ext
                                .get_or_default::<PlayerStats>()
                                .interval(interval)
                                .clear();
                        }
                        mirror(z, p);
                    }),
                },
            );
        }
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
        register_flag_stats(zone, me);
        register_ball_stats(zone, me);
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
            let s = KillSettings::load(&cfg);
            let carried = killer.flags_carried;
            let killer_bounty = i32::from(killer.pos.bounty);
            let team_kill = killer.freq == killed.freq;
            let team_flags =
                super::flagcore::count_freq_flags(z, k.arena, killer.freq as i16) as i32;
            let pts = kill_points(
                i32::from(k.bounty),
                i32::from(k.flags),
                i32::from(carried),
                team_flags,
                team_kill,
                killer_bounty,
                &s,
            );
            k.points.set(k.points.get() + pts);
        });
        Ok(())
    }
}

/// basicstats' flag counters (ASSS `myflaggain`/`myflaglost`/`myflagreset`).
fn register_flag_stats(zone: &mut Zone, me: Owner) {
    use super::flagcore::{Cleanup, FlagGain, FlagLost, FlagReset, GainHow};
    zone.on::<FlagGain>(me, Scope::Global, |z, ev| {
        if ev.how == GainHow::Pickup
            && let Some(st) = z.iface::<dyn Stats>()
        {
            st.increment(z, ev.player, stat::FLAG_PICKUPS, 1);
        }
    });
    zone.on::<FlagLost>(me, Scope::Global, |z, ev| {
        let Some(st) = z.iface::<dyn Stats>() else {
            return;
        };
        match ev.reason {
            Cleanup::Dropped | Cleanup::InSafe => st.increment(z, ev.player, stat::FLAG_DROPS, 1),
            Cleanup::ShipChange | Cleanup::FreqChange | Cleanup::LeftArena => {
                st.increment(z, ev.player, stat::FLAG_NEUT_DROPS, 1);
            }
            _ => {}
        }
    });
    zone.on::<FlagReset>(me, Scope::Global, |z, ev| {
        if ev.freq < 0 || ev.points <= 0 {
            return;
        }
        let Some(st) = z.iface::<dyn Stats>() else {
            return;
        };
        let members: Vec<(PlayerId, bool, bool)> = z
            .players
            .in_arena(ev.arena)
            .filter(|p| !p.ship.is_spec())
            .map(|p| {
                let safe = p.pos.status & oss_proto::position::status::SAFEZONE != 0;
                (p.id, p.freq as i16 == ev.freq, safe)
            })
            .collect();
        for (pid, won, safe) in members {
            if won {
                st.increment(z, pid, stat::FLAG_GAMES_WON, 1);
                // no flag reward in a safe zone
                if !safe {
                    st.increment(z, pid, stat::FLAG_POINTS, ev.points);
                }
            } else {
                st.increment(z, pid, stat::FLAG_GAMES_LOST, 1);
            }
        }
    });
    zone.on::<super::fg_turf::TurfTag>(me, Scope::Global, |z, ev| {
        if let Some(st) = z.iface::<dyn Stats>() {
            st.increment(z, ev.player, stat::TURF_TAGS, 1);
        }
    });
}

/// basicstats' ball counters (ASSS `mybpickup`/`mygoal`).
pub(crate) fn register_ball_stats(zone: &mut Zone, me: Owner) {
    use super::balls::{BallPickup, Goal};
    zone.on::<BallPickup>(me, Scope::Global, |z, ev| {
        if let Some(st) = z.iface::<dyn Stats>() {
            st.increment(z, ev.player, stat::BALL_CARRIES, 1);
        }
    });
    zone.on::<Goal>(me, Scope::Global, |z, ev| {
        if let Some(st) = z.iface::<dyn Stats>() {
            st.increment(z, ev.player, stat::BALL_GOALS, 1);
        }
    });
}

/// The `Kill:*` settings points_kill uses.
#[derive(Debug, Clone, Copy)]
pub struct KillSettings {
    /// `Kill:FixedKillReward` (-1 = use the bounty).
    pub fixed_reward: i32,
    /// `Kill:FlagMinimumBounty`.
    pub flag_min_bounty: i32,
    /// `Kill:PointsPerKilledFlag` (ASSS's code default is 0, despite its help text).
    pub per_killed_flag: i32,
    /// `Kill:PointsPerCarriedFlag`.
    pub per_carried_flag: i32,
    /// `Kill:PointsPerTeamFlag`.
    pub per_team_flag: i32,
    /// `Misc:TeamKillPoints`.
    pub team_kill_points: bool,
}

impl KillSettings {
    /// Reads the settings from an arena config.
    pub fn load(cfg: &crate::config::ConfigFile) -> Self {
        Self {
            fixed_reward: cfg.get_int("Kill", "FixedKillReward", -1),
            flag_min_bounty: cfg.get_int("Kill", "FlagMinimumBounty", 0),
            per_killed_flag: cfg.get_int("Kill", "PointsPerKilledFlag", 0),
            per_carried_flag: cfg.get_int("Kill", "PointsPerCarriedFlag", 0),
            per_team_flag: cfg.get_int("Kill", "PointsPerTeamFlag", 0),
            team_kill_points: cfg.get_int("Misc", "TeamKillPoints", 0) != 0,
        }
    }
}

/// ASSS `points_kill.c`: points for a kill. The flag bonuses are gated on
/// the killer's current position bounty; a team kill scores 0 (bonuses too)
/// unless `Misc:TeamKillPoints`.
pub fn kill_points(
    bounty: i32,
    transferred_flags: i32,
    killer_carried: i32,
    team_flags: i32,
    team_kill: bool,
    killer_bounty: i32,
    s: &KillSettings,
) -> i32 {
    let mut pts = if s.fixed_reward != -1 {
        s.fixed_reward
    } else {
        bounty
    };
    if killer_bounty >= s.flag_min_bounty {
        if transferred_flags != 0 {
            pts += transferred_flags * s.per_killed_flag;
        }
        if killer_carried != 0 {
            pts += killer_carried * s.per_carried_flag;
        }
        if team_flags != 0 {
            pts += team_flags * s.per_team_flag;
        }
    }
    if team_kill && !s.team_kill_points {
        pts = 0;
    }
    pts
}
