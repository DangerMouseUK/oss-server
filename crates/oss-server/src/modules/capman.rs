//! `capman`: groups and capabilities (ASSS `capman.c`).
//!
//! A player's group comes from `conf/staff.conf` (section = arena base name,
//! then `(global)`), but only for authenticated players; everyone else is in
//! `default`. A group has a capability if `conf/groupdef.conf` lists it in the
//! group's section.

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::core::{Action, NewPlayer, PlayerAction};

/// The default group.
pub const DEFAULT_GROUP: &str = "default";
/// `staff.conf` section for zone-wide groups (ASSS `AG_GLOBAL`).
pub const GLOBAL_SECTION: &str = "(global)";
/// Sees private frequencies; also marks staff in the login response.
pub const CAP_SEE_PRIV_FREQ: &str = "seeprivfreq";
/// May send sound messages.
pub const CAP_SOUND_MESSAGES: &str = "sendsoundmessages";
/// May send staff chat.
pub const CAP_SEND_MOD_CHAT: &str = "sendmodchat";
/// Receives staff chat.
pub const CAP_MOD_CHAT: &str = "seemodchat";
/// May look into private arenas (`#name`).
pub const CAP_SEE_PRIV_ARENA: &str = "seeprivarena";

/// Capability checks (ASSS `Icapman`).
pub trait Capabilities: Send + Sync {
    /// Does the player's current group have `cap`?
    fn has_capability(&self, zone: &mut Zone, player: PlayerId, cap: &str) -> bool;
    /// Would the player have `cap` in `arena`?
    fn has_capability_in_arena(
        &self,
        zone: &mut Zone,
        player: PlayerId,
        arena: ArenaId,
        cap: &str,
    ) -> bool;
    /// Does the global group of `name` have `cap`?
    fn has_capability_by_name(&self, zone: &mut Zone, name: &str, cap: &str) -> bool;
    /// Is `a` in a group higher than `b`'s (`higher_than_<group>`)?
    fn higher_than(&self, zone: &mut Zone, a: PlayerId, b: PlayerId) -> bool;
}

/// Group management (ASSS `Igroupman`).
pub trait Groups: Send + Sync {
    /// The player's current group.
    fn group(&self, zone: &Zone, player: PlayerId) -> String;
    /// Sets the group for this session only.
    fn set_temp_group(&self, zone: &mut Zone, player: PlayerId, group: &str);
    /// Sets the group permanently (globally or for the player's arena).
    fn set_perm_group(
        &self,
        zone: &mut Zone,
        player: PlayerId,
        group: &str,
        global: bool,
        info: &str,
    );
    /// Moves the player back to `default`, permanently.
    fn remove_group(&self, zone: &mut Zone, player: PlayerId, info: &str);
    /// Checks a group password from `staff.conf` `[GroupPasswords]`.
    fn check_group_password(&self, zone: &mut Zone, group: &str, password: &str) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Source {
    #[default]
    Default,
    Global,
    Arena,
    Temp,
}

#[derive(Debug, Clone)]
struct GroupData {
    group: String,
    source: Source,
}

impl Default for GroupData {
    fn default() -> Self {
        Self {
            group: "none".to_owned(),
            source: Source::Default,
        }
    }
}

fn files(
    zone: &mut Zone,
) -> (
    Option<std::sync::Arc<crate::config::ConfigFile>>,
    Option<std::sync::Arc<crate::config::ConfigFile>>,
) {
    (
        zone.cfg.open(None, "groupdef.conf").ok(),
        zone.cfg.open(None, "staff.conf").ok(),
    )
}

/// ASSS `update_group`.
fn compute_group(zone: &mut Zone, player: PlayerId, arena: Option<ArenaId>) -> GroupData {
    let Some(p) = zone.players.get(player) else {
        return GroupData::default();
    };
    if !p.authenticated {
        return GroupData {
            group: DEFAULT_GROUP.to_owned(),
            source: Source::Default,
        };
    }
    let name = p.name.clone();
    let base = arena
        .and_then(|a| zone.arenas.get(a))
        .map(|a| a.basename.clone());
    let (_, staff) = files(zone);
    let Some(staff) = staff else {
        return GroupData {
            group: DEFAULT_GROUP.to_owned(),
            source: Source::Default,
        };
    };
    if let Some(g) = base.as_deref().and_then(|b| staff.get(b, &name)) {
        return GroupData {
            group: g.to_owned(),
            source: Source::Arena,
        };
    }
    if let Some(g) = staff.get(GLOBAL_SECTION, &name) {
        return GroupData {
            group: g.to_owned(),
            source: Source::Global,
        };
    }
    GroupData {
        group: DEFAULT_GROUP.to_owned(),
        source: Source::Default,
    }
}

fn group_has(zone: &mut Zone, group: &str, cap: &str) -> bool {
    files(zone).0.is_some_and(|g| g.get(group, cap).is_some())
}

fn current(zone: &Zone, player: PlayerId) -> GroupData {
    zone.players
        .get(player)
        .and_then(|p| p.ext.get::<GroupData>().cloned())
        .unwrap_or_default()
}

fn store(zone: &mut Zone, player: PlayerId, g: GroupData) {
    if let Some(p) = zone.players.get_mut(player) {
        p.ext.insert(g);
    }
}

/// The capman implementation.
pub struct CapMan;

impl Capabilities for CapMan {
    fn has_capability(&self, zone: &mut Zone, player: PlayerId, cap: &str) -> bool {
        let g = current(zone, player).group;
        group_has(zone, &g, cap)
    }

    fn has_capability_in_arena(
        &self,
        zone: &mut Zone,
        player: PlayerId,
        arena: ArenaId,
        cap: &str,
    ) -> bool {
        let g = compute_group(zone, player, Some(arena)).group;
        group_has(zone, &g, cap)
    }

    fn has_capability_by_name(&self, zone: &mut Zone, name: &str, cap: &str) -> bool {
        let group = files(zone)
            .1
            .and_then(|s| s.get(GLOBAL_SECTION, name).map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_GROUP.to_owned());
        group_has(zone, &group, cap)
    }

    fn higher_than(&self, zone: &mut Zone, a: PlayerId, b: PlayerId) -> bool {
        let bg = current(zone, b).group;
        self.has_capability(zone, a, &format!("higher_than_{bg}"))
    }
}

impl Groups for CapMan {
    fn group(&self, zone: &Zone, player: PlayerId) -> String {
        current(zone, player).group
    }

    fn set_temp_group(&self, zone: &mut Zone, player: PlayerId, group: &str) {
        store(
            zone,
            player,
            GroupData {
                group: group.to_owned(),
                source: Source::Temp,
            },
        );
    }

    fn set_perm_group(
        &self,
        zone: &mut Zone,
        player: PlayerId,
        group: &str,
        global: bool,
        info: &str,
    ) {
        let Some(p) = zone.players.get(player) else {
            return;
        };
        let name = p.name.clone();
        let base = p
            .arena
            .and_then(|a| zone.arenas.get(a))
            .map(|a| a.basename.clone());
        let mut g = current(zone, player);
        g.group = group.to_owned();
        let section = if global {
            g.source = Source::Global;
            Some(GLOBAL_SECTION.to_owned())
        } else if let Some(b) = base {
            g.source = Source::Arena;
            Some(b)
        } else {
            None
        };
        store(zone, player, g);
        if let (Some(sec), Some(staff)) = (section, files(zone).1) {
            zone.cfg.set(&staff, &sec, &name, group, Some(info), true);
        }
    }

    fn remove_group(&self, zone: &mut Zone, player: PlayerId, info: &str) {
        let Some(p) = zone.players.get(player) else {
            return;
        };
        let Some(arena) = p.arena else { return };
        let name = p.name.clone();
        let base = zone
            .arenas
            .get(arena)
            .map(|a| a.basename.clone())
            .unwrap_or_default();
        let old = current(zone, player);
        store(
            zone,
            player,
            GroupData {
                group: DEFAULT_GROUP.to_owned(),
                source: old.source,
            },
        );
        let section = match old.source {
            Source::Global => Some(GLOBAL_SECTION.to_owned()),
            Source::Arena => Some(base),
            Source::Default | Source::Temp => None,
        };
        if let (Some(sec), Some(staff)) = (section, files(zone).1) {
            zone.cfg
                .set(&staff, &sec, &name, DEFAULT_GROUP, Some(info), true);
        }
    }

    fn check_group_password(&self, zone: &mut Zone, group: &str, password: &str) -> bool {
        files(zone)
            .1
            .and_then(|s| s.get("GroupPasswords", group).map(|pw| pw == password))
            .unwrap_or(false)
    }
}

/// The `capman` module.
pub struct CapManModule;

impl Module for CapManModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let cm = std::sync::Arc::new(CapMan);
        zone.reg
            .provide::<dyn Capabilities>(me, Scope::Global, cm.clone());
        zone.reg.provide::<dyn Groups>(me, Scope::Global, cm);
        zone.on::<NewPlayer>(me, Scope::Global, |z, ev| {
            if ev.created {
                store(z, ev.player, GroupData::default());
            }
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| match ev.action {
            Action::PreEnterArena => {
                let g = compute_group(z, ev.player, ev.arena);
                tracing::debug!(target: "capman", pid = ev.player.0, group = %g.group, "assigned group (arena)");
                store(z, ev.player, g);
            }
            Action::Connect => {
                let g = compute_group(z, ev.player, None);
                store(z, ev.player, g);
            }
            Action::Disconnect | Action::LeaveArena => {
                store(z, ev.player, GroupData::default());
            }
            Action::EnterArena => {}
        });
        Ok(())
    }
}
