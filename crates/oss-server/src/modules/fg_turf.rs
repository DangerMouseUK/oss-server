//! `fg_turf`: the turf flag game (attach to arenas; ASSS `fg_turf.py`).
//!
//! The map's static flags (tile 170) stay where they are; touching one
//! claims it for your team. Owners are broadcast every 30 seconds and on
//! every claim. Not yet ported: keeping owners across restarts
//! (`Flag:PersistentTurfOwners`, needs the persist module).

use std::sync::Arc;

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::flagcore::{self, CARRY_NONE, Cleanup, FlagGame, FlagInfo};

/// ASSS `CB_TURFTAG`: a player claimed a turf flag.
#[derive(Debug, Clone, Copy)]
pub struct TurfTag {
    /// Arena.
    pub arena: ArenaId,
    /// Player.
    pub player: PlayerId,
    /// Flag id.
    pub fid: usize,
    /// Previous owner (-1 = none).
    pub old_freq: i16,
    /// New owner.
    pub new_freq: i16,
}

impl Event for TurfTag {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// The turf game.
pub struct Turf;

impl FlagGame for Turf {
    fn init(&self, zone: &mut Zone, arena: ArenaId) {
        let count = super::mapdata::arena_map(zone, arena).map_or(0, |m| m.turf_flags);
        flagcore::set_carry_mode(zone, arena, CARRY_NONE);
        flagcore::reserve_flags(zone, arena, count);
        for fid in 0..count {
            flagcore::set_flag(zone, arena, fid, FlagInfo::on_map(-1, -1, -1));
        }
    }

    fn flag_touch(&self, zone: &mut Zone, arena: ArenaId, player: PlayerId, fid: usize) {
        let Some(f) = flagcore::get_flag(zone, arena, fid) else {
            return;
        };
        let Some(new_freq) = zone.players.get(player).map(|p| p.freq as i16) else {
            return;
        };
        flagcore::set_flag(zone, arena, fid, FlagInfo::on_map(-1, -1, new_freq));
        zone.fire(&TurfTag {
            arena,
            player,
            fid,
            old_freq: f.freq,
            new_freq,
        });
    }

    fn cleanup(&self, _: &mut Zone, _: ArenaId, _: usize, _: Cleanup, _: PlayerId, _: i16) {}
}

/// The `fg_turf` module (attach to arenas).
pub struct FgTurfModule;

impl Module for FgTurfModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn FlagGame>(me, Scope::Arena(arena), Arc::new(Turf));
        Ok(())
    }
}
