//! `periodic` and `points_periodic`: periodic flag-ownership rewards (ASSS
//! `scoring/periodic.c`, `scoring/points_periodic.c`).
//!
//! Every `Periodic:RewardDelay` ticks (0 = off), if at least
//! `Periodic:RewardMinimumPlayers` are in the arena, each team is rewarded
//! for the flags it owns: S2C `0x23` lists the points per team, and every
//! team member in a ship and outside a safe zone gets them as flag points.
//! The points come from a [`PeriodicPoints`] provider attached to the arena;
//! `points_periodic` gives `Periodic:RewardPoints` per flag (or, if
//! negative, its absolute value per flag per player in the arena).

use std::collections::BTreeMap;
use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::s2c::{PeriodicReward, Reward};

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};
use super::stats::{Stats, stat};

/// Computes a team's periodic reward (ASSS `Iperiodicpoints`).
pub trait PeriodicPoints: Send + Sync {
    /// Points for each member of `freq`.
    fn points(
        &self,
        zone: &Zone,
        arena: ArenaId,
        freq: u16,
        freq_players: usize,
        total_players: usize,
        flags: usize,
    ) -> i32;
}

struct Basic;

impl PeriodicPoints for Basic {
    fn points(
        &self,
        zone: &Zone,
        arena: ArenaId,
        _: u16,
        _: usize,
        total: usize,
        flags: usize,
    ) -> i32 {
        let rw = zone
            .arenas
            .get(arena)
            .map_or(100, |a| a.cfg.get_int("Periodic", "RewardPoints", 100));
        periodic_points(rw, flags as i32, total as i32)
    }
}

/// ASSS `points_periodic.c`: `flags × RewardPoints` when positive, else
/// `flags × -RewardPoints × players in the arena`.
pub fn periodic_points(reward_points: i32, flags: i32, total_players: i32) -> i32 {
    if reward_points > 0 {
        flags * reward_points
    } else {
        flags * -reward_points * total_players
    }
}

struct Timer(Option<crate::timers::TimerId>);

fn reward(zone: &mut Zone, a: ArenaId) -> bool {
    let Some(pp) = zone.iface_in::<dyn PeriodicPoints>(a) else {
        return true;
    };
    let min = zone
        .arenas
        .get(a)
        .map_or(0, |x| x.cfg.get_int("Periodic", "RewardMinimumPlayers", 0));
    let mut freqs: BTreeMap<u16, usize> = BTreeMap::new();
    for p in zone.players.in_arena(a) {
        *freqs.entry(p.freq).or_default() += 1;
    }
    let total: usize = freqs.values().sum();
    if (total as i64) < i64::from(min) {
        return true;
    }
    let mut table = BTreeMap::new();
    let mut rewards = Vec::new();
    for (&freq, &n) in &freqs {
        let flags = super::flagcore::count_freq_flags(zone, a, freq as i16);
        let pts = pp.points(zone, a, freq, n, total, flags);
        table.insert(freq, pts);
        rewards.push(Reward {
            freq: freq as i16,
            points: pts as i16,
        });
    }
    let mut buf = Vec::with_capacity(1 + 4 * rewards.len());
    PeriodicReward { rewards }.encode(&mut buf);
    zone.send_to_arena(a, None, &buf, SendFlags::RELIABLE);
    let winners: Vec<(PlayerId, i32)> = zone
        .players
        .in_arena(a)
        .filter(|p| {
            matches!(p.kind, ClientKind::Vie | ClientKind::Continuum)
                && !p.ship.is_spec()
                && p.pos.status & oss_proto::position::status::SAFEZONE == 0
        })
        .filter_map(|p| table.get(&p.freq).map(|&pts| (p.id, pts)))
        .collect();
    if let Some(st) = zone.iface::<dyn Stats>() {
        for (pid, pts) in winners {
            st.increment(zone, pid, stat::FLAG_POINTS, pts);
        }
    }
    true
}

/// The `periodic` module.
pub struct PeriodicModule;

impl Module for PeriodicModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on::<ArenaAction>(me, Scope::Global, move |z, ev| {
            let a = ev.arena;
            match ev.action {
                ArenaActionKind::Create | ArenaActionKind::ConfChanged => {
                    if let Some(t) = z
                        .arenas
                        .get_mut(a)
                        .and_then(|x| x.ext.remove::<Timer>())
                        .and_then(|t| t.0)
                    {
                        z.timers.cancel(t);
                    }
                    let delay = z
                        .arenas
                        .get(a)
                        .map_or(0, |x| x.cfg.get_int("Periodic", "RewardDelay", 0));
                    let t = u64::try_from(delay).ok().filter(|&d| d > 0).map(|d| {
                        z.set_timer(me, d, d, move |z| z.arenas.get(a).is_some() && reward(z, a))
                    });
                    if let Some(x) = z.arenas.get_mut(a) {
                        x.ext.insert(Timer(t));
                    }
                }
                ArenaActionKind::Destroy => {
                    if let Some(t) = z
                        .arenas
                        .get_mut(a)
                        .and_then(|x| x.ext.remove::<Timer>())
                        .and_then(|t| t.0)
                    {
                        z.timers.cancel(t);
                    }
                }
                _ => {}
            }
        });
        Ok(())
    }
}

/// The `points_periodic` module (attach to arenas).
pub struct PointsPeriodicModule;

impl Module for PointsPeriodicModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn PeriodicPoints>(me, Scope::Arena(arena), Arc::new(Basic));
        Ok(())
    }
}
