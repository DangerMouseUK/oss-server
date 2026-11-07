//! `clientset`: keeps each arena's settings packet and sends it (ASSS
//! `clientset.c` module side; the packet itself is built by
//! [`crate::clientset`]).

use std::sync::Arc;

use crate::arena::ArenaId;
use crate::clientset::{self, ArenaSettings, OverrideKey, Overrides};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind, SendSettings};
use super::core::{Action, PlayerAction};

/// Settings overrides, checksums and random prizes (ASSS `Iclientset`).
pub trait ClientSettings: SendSettings {
    /// Overrides a field for a whole arena (takes effect on the next send).
    fn arena_override(&self, zone: &mut Zone, arena: ArenaId, key: OverrideKey, value: i32);
    /// Removes an arena override.
    fn arena_unoverride(&self, zone: &mut Zone, arena: ArenaId, key: OverrideKey);
    /// Overrides a field for one player until they change arena.
    fn player_override(&self, zone: &mut Zone, player: PlayerId, key: OverrideKey, value: i32);
    /// Removes a player override.
    fn player_unoverride(&self, zone: &mut Zone, player: PlayerId, key: OverrideKey);
    /// The settings checksum the player's client should report, or `None` if
    /// the player isn't playing.
    fn checksum(&self, zone: &Zone, player: PlayerId, key: u32) -> Option<u32>;
    /// A weighted random prize for the arena (0 if all weights are 0).
    fn random_prize(
        &self,
        zone: &Zone,
        arena: ArenaId,
        rand_below: &mut dyn FnMut(u32) -> u32,
    ) -> u8;
}

#[derive(Default)]
struct ArenaCs {
    settings: ArenaSettings,
    overrides: Overrides,
}

#[derive(Default)]
struct PlayerCs {
    overrides: Option<Box<Overrides>>,
}

fn effective(zone: &Zone, player: PlayerId) -> Option<clientset::SettingsBlock> {
    let p = zone.players.get(player)?;
    let a = zone.arenas.get(p.arena?)?;
    let cs = a.ext.get::<ArenaCs>()?;
    if cs.settings.block.as_bytes()[0] != clientset::S2C_SETTINGS {
        return None;
    }
    let po = p.ext.get::<PlayerCs>().and_then(|d| d.overrides.as_deref());
    Some(cs.settings.block.with_overrides(&cs.overrides, po))
}

fn load(zone: &mut Zone, arena: ArenaId) -> bool {
    let Some(a) = zone.arenas.get_mut(arena) else {
        return false;
    };
    let new = clientset::load(&a.cfg);
    let cs = a.ext.get_or_default::<ArenaCs>();
    let changed = cs.settings != new;
    cs.settings = new;
    changed
}

/// The clientset implementation.
pub struct ClientSet;

impl SendSettings for ClientSet {
    fn send_settings(&self, zone: &mut Zone, player: PlayerId) {
        if let Some(block) = effective(zone, player) {
            zone.send(player, block.as_bytes(), SendFlags::RELIABLE);
        }
    }
}

impl ClientSettings for ClientSet {
    fn arena_override(&self, zone: &mut Zone, arena: ArenaId, key: OverrideKey, value: i32) {
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.get_or_default::<ArenaCs>().overrides.set(key, value);
        }
    }

    fn arena_unoverride(&self, zone: &mut Zone, arena: ArenaId, key: OverrideKey) {
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.get_or_default::<ArenaCs>().overrides.unset(key);
        }
    }

    fn player_override(&self, zone: &mut Zone, player: PlayerId, key: OverrideKey, value: i32) {
        if let Some(p) = zone.players.get_mut(player) {
            p.ext
                .get_or_default::<PlayerCs>()
                .overrides
                .get_or_insert_with(Default::default)
                .set(key, value);
        }
    }

    fn player_unoverride(&self, zone: &mut Zone, player: PlayerId, key: OverrideKey) {
        if let Some(o) = zone
            .players
            .get_mut(player)
            .and_then(|p| p.ext.get_mut::<PlayerCs>())
            .and_then(|d| d.overrides.as_mut())
        {
            o.unset(key);
        }
    }

    fn checksum(&self, zone: &Zone, player: PlayerId, key: u32) -> Option<u32> {
        if zone.players.get(player)?.status != PlayerStatus::Playing {
            return None;
        }
        effective(zone, player).map(|b| b.checksum(key))
    }

    fn random_prize(
        &self,
        zone: &Zone,
        arena: ArenaId,
        rand_below: &mut dyn FnMut(u32) -> u32,
    ) -> u8 {
        zone.arenas
            .get(arena)
            .and_then(|a| a.ext.get::<ArenaCs>())
            .map_or(0, |cs| cs.settings.prize_weights.random_prize(rand_below))
    }
}

/// The `clientset` module.
pub struct ClientSetModule;

impl Module for ClientSetModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let cs = Arc::new(ClientSet);
        zone.reg
            .provide::<dyn SendSettings>(me, Scope::Global, cs.clone());
        zone.reg
            .provide::<dyn ClientSettings>(me, Scope::Global, cs);
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create => {
                load(z, ev.arena);
            }
            ArenaActionKind::ConfChanged => {
                if load(z, ev.arena) {
                    tracing::info!(target: "clientset", arena = ev.arena.0, "sending modified settings");
                    let ids: Vec<PlayerId> = z.players.in_arena(ev.arena).map(|p| p.id).collect();
                    for pid in ids {
                        ClientSet.send_settings(z, pid);
                    }
                }
            }
            ArenaActionKind::Destroy => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.remove::<ArenaCs>();
                }
            }
            _ => {}
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if matches!(ev.action, Action::LeaveArena | Action::Disconnect)
                && let Some(p) = z.players.get_mut(ev.player)
            {
                p.ext.remove::<PlayerCs>();
            }
        });
        Ok(())
    }
}
