//! `arenaperm`: keeps players out of arenas they have no capability for
//! (ASSS `arenaperm.c`).
//!
//! An arena with `General:NeedCap` set may only be entered by players whose
//! group has that capability. Someone who asks for such an arena is told so
//! and sent to the first arena they are allowed into instead; if there is no
//! such arena, the attempt is logged and they stay where they were sent, as
//! in ASSS.
//!
//! Load this globally (it is not an arena-attached module): the check runs
//! for every arena entry in the zone.

use crate::arena::{ArenaId, ArenaStatus};
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::capman::Capabilities;
use super::core::{Action, PlayerAction};

/// Whether `player` may enter `arena`: a running arena with no
/// `General:NeedCap`, or one whose capability they have.
pub fn has_permission(zone: &mut Zone, player: PlayerId, arena: ArenaId) -> bool {
    let needed = match zone.arenas.get(arena) {
        Some(a) if a.status == ArenaStatus::Running || a.status == ArenaStatus::DoInit0 => {
            a.cfg.get("General", "NeedCap").map(str::to_owned)
        }
        // an arena that isn't up yet is nobody's to enter, as in ASSS
        _ => return false,
    };
    let Some(cap) = needed else { return true };
    zone.iface::<dyn Capabilities>()
        .is_some_and(|c| c.has_capability(zone, player, &cap))
}

fn on_pre_enter(zone: &mut Zone, player: PlayerId, arena: ArenaId) {
    if has_permission(zone, player, arena) {
        return;
    }
    let name = zone
        .arenas
        .get(arena)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let elsewhere = zone
        .arenas
        .ids()
        .into_iter()
        .filter(|&id| id != arena)
        .find(|&id| has_permission(zone, player, id));
    let Some(other) = elsewhere else {
        let who = zone
            .players
            .get(player)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        tracing::warn!(target: "arenaperm", %who, arena = %name, "no unrestricted arena to send them to");
        return;
    };
    let to = zone
        .arenas
        .get(other)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    if let Some(p) = zone.players.get_mut(player) {
        p.arena = Some(other);
    }
    super::chat::message(
        zone,
        player,
        &format!("You don't have permission to enter arena {name}!"),
    );
    tracing::info!(target: "arenaperm", arena = %name, %to, pid = player.0, "redirected");
}

/// The `arenaperm` module.
pub struct ArenaPermModule;

impl Module for ArenaPermModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action == Action::PreEnterArena
                && let Some(arena) = ev.arena
            {
                on_pre_enter(z, ev.player, arena);
            }
        });
        Ok(())
    }
}
