//! `jackpot` and `points_flag` (ASSS `scoring/jackpot.c`,
//! `scoring/points_flag.c`).
//!
//! The jackpot grows by `Kill:JackpotBountyPercent` (tenths of a percent) of
//! each killed player's bounty and resets when a flag game ends. `points_flag`
//! (attach to arenas) sets the warzone reward to
//! `players² × Flag:FlagReward / 1000` plus the jackpot, optionally split
//! among the winning team (`Flag:SplitPoints`).

use std::sync::Arc;

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::player::{ClientKind, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::fg_wz::WarzoneWin;
use super::flagcore::FlagReset;
use super::game::Kill;

/// The arena jackpot (ASSS `Ijackpot`).
pub trait Jackpot: Send + Sync {
    /// Current jackpot.
    fn get(&self, zone: &Zone, arena: ArenaId) -> i32;
    /// Sets it.
    fn set(&self, zone: &mut Zone, arena: ArenaId, points: i32);
    /// Resets it to 0.
    fn reset(&self, zone: &mut Zone, arena: ArenaId) {
        self.set(zone, arena, 0);
    }
}

#[derive(Default)]
struct Jp(i32);

/// The jackpot implementation.
pub struct JackpotImpl;

impl Jackpot for JackpotImpl {
    fn get(&self, zone: &Zone, arena: ArenaId) -> i32 {
        zone.arenas
            .get(arena)
            .and_then(|a| a.ext.get::<Jp>())
            .map_or(0, |j| j.0)
    }

    fn set(&self, zone: &mut Zone, arena: ArenaId, points: i32) {
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.get_or_default::<Jp>().0 = points;
        }
    }
}

/// The `jackpot` module.
pub struct JackpotModule;

impl Module for JackpotModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Jackpot>(me, Scope::Global, Arc::new(JackpotImpl));
        zone.on::<Kill>(me, Scope::Global, |z, k| {
            let Some(a) = z.arenas.get_mut(k.arena) else {
                return;
            };
            let percent = a.cfg.get_int("Kill", "JackpotBountyPercent", 0);
            if percent != 0 {
                let jp = a.ext.get_or_default::<Jp>();
                jp.0 = jp.0.saturating_add(i32::from(k.bounty) * percent / 1000);
            }
        });
        zone.on::<FlagReset>(me, Scope::Global, |z, ev| JackpotImpl.reset(z, ev.arena));
        Ok(())
    }
}

/// The `points_flag` module (attach to arenas).
pub struct PointsFlagModule;

impl Module for PointsFlagModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.on::<WarzoneWin>(me, Scope::Arena(arena), |z, ev| {
            let (mut players, mut on_freq) = (0i64, 0i64);
            for p in z.players.iter() {
                if p.status == PlayerStatus::Playing
                    && p.arena == Some(ev.arena)
                    && !p.ship.is_spec()
                    && p.kind != ClientKind::Fake
                {
                    players += 1;
                    if p.freq as i16 == ev.freq {
                        on_freq += 1;
                    }
                }
            }
            let Some(cfg) = z.arenas.get(ev.arena).map(|a| a.cfg.clone()) else {
                return;
            };
            let mut points = i64::from(warzone_reward(
                players as i32,
                cfg.get_int("Flag", "FlagReward", 5000),
            ));
            if let Some(jp) = z.iface::<dyn Jackpot>() {
                points += i64::from(jp.get(z, ev.arena));
            }
            if on_freq > 0 && cfg.get_int("Flag", "SplitPoints", 0) != 0 {
                points /= on_freq;
            }
            // Deliberate difference: ASSS adds the jackpot in 32-bit ints
            // with no clamp, so an absurd jackpot or FlagReward can wrap into
            // a negative award. warzone_reward() still wraps like ASSS (that
            // part is pinned by vectors); the total here is kept in 0..=i32::MAX.
            ev.points.set(
                ev.points
                    .get()
                    .saturating_add(points.clamp(0, i64::from(i32::MAX)) as i32),
            );
        });
        Ok(())
    }
}

/// ASSS `points_flag.c`: `players² × Flag:FlagReward / 1000` in 32-bit
/// integer arithmetic, where `players` is everyone in the arena who is in a
/// ship (not just the winning team).
pub fn warzone_reward(players: i32, flag_reward: i32) -> i32 {
    players.wrapping_mul(players).wrapping_mul(flag_reward) / 1000
}
