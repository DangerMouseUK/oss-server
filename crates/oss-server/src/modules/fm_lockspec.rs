//! `fm_lockspec`: a frequency manager that keeps everyone in spectator mode
//! (ASSS `fm_lockspec.c`).
//!
//! Attach it to an arena and every answer — entering, changing ship,
//! changing frequency — is the spectator frequency in spectator mode. ASSS's
//! version hands back `CFG_DEF_SPEC_FREQ` (8025) whatever the arena's
//! `Team:SpecFreq` says; this one uses the arena's own spectator frequency,
//! which is the same number unless a zone changed it.
//!
//! It replaces [`super::game::BalancedFreqs`] for the arenas it is attached
//! to, because an arena-scoped interface wins over the global one.

use std::sync::Arc;

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, Ship};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::core::FreqManager;

struct LockSpec;

impl LockSpec {
    fn spec(&self, zone: &Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        *ship = Ship::SPEC;
        *freq = zone
            .players
            .get(player)
            .and_then(|p| p.arena)
            .and_then(|a| zone.arenas.get(a))
            .map_or(8025, super::arenaman::spec_freq);
    }
}

impl FreqManager for LockSpec {
    fn initial_freq(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        self.spec(zone, player, ship, freq);
    }

    fn ship_change(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        self.spec(zone, player, ship, freq);
    }

    fn freq_change(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        self.spec(zone, player, ship, freq);
    }
}

/// The `fm_lockspec` module (attach to arenas).
pub struct FmLockSpecModule;

impl Module for FmLockSpecModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn FreqManager>(me, Scope::Arena(arena), Arc::new(LockSpec));
        Ok(())
    }
}
