//! `watchdamage`: watching the damage another player takes (ASSS
//! `watchdamage.c`).
//!
//! A Continuum client only reports damage while the server asks it to, so
//! the server turns reporting on (S2C `0x37`) for a player as soon as
//! somebody watches them and off again when the last watcher goes. Each
//! report (C2S `0x32`) is relayed to that player's watchers as S2C `0x38`.
//!
//! `?watchdamage` aimed at a player turns watching them on, off, or over
//! (`1`, `0`, or no argument); typed in public, `?watchdamage 0` stops
//! watching everyone. `?watchwatchdamage` says who is reporting damage and
//! who is watching whom.
//!
//! Modules can watch too, through [`module_watch`], which counts separately
//! so that a module and a player watching the same person don't turn each
//! other off. A damage report raises [`PlayerDamage`] only while a module is
//! watching, as in ASSS: these packets arrive in bulk and firing an event
//! for every one of them would cost every arena that never uses it.

use oss_proto::Packet;
use oss_proto::c2s::{self, C2SDamage};
use oss_proto::common::DamageData;
use oss_proto::s2c::{S2CDamage, ToggleDamage};

use crate::arena::ArenaId;
use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus};
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::chat::message;
use super::core::{Action, PlayerAction};

/// ASSS `CB_PLAYERDAMAGE`: a watched player reported damage. Raised only
/// while a module is watching them ([`module_watch`]).
#[derive(Debug, Clone)]
pub struct PlayerDamage {
    /// The arena they are in.
    pub arena: ArenaId,
    /// Who took the damage.
    pub player: PlayerId,
    /// Server time of the report.
    pub time: u32,
    /// What hit them.
    pub entries: Vec<DamageData>,
}

impl Event for PlayerDamage {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// Who is watching this player, and how many modules are.
#[derive(Default)]
struct Watch {
    players: Vec<PlayerId>,
    modules: u16,
}

impl Watch {
    fn count(&self) -> usize {
        self.players.len() + usize::from(self.modules)
    }
}

fn watch(zone: &mut Zone, p: PlayerId) -> Option<&mut Watch> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<Watch>())
}

/// How many watchers a player has (ASSS `WatchCount`).
pub fn watch_count(zone: &Zone, p: PlayerId) -> usize {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<Watch>())
        .map_or(0, Watch::count)
}

/// Tells a Continuum client to start or stop reporting damage.
fn toggle(zone: &mut Zone, target: PlayerId, on: bool) {
    if zone.players.get(target).map(|p| p.kind) != Some(ClientKind::Continuum) {
        return;
    }
    let mut buf = Vec::with_capacity(2);
    ToggleDamage { on: u8::from(on) }.encode(&mut buf);
    zone.send(target, &buf, SendFlags::RELIABLE);
}

/// Starts watching `target`. False if `watcher` already was.
pub fn add_watch(zone: &mut Zone, watcher: PlayerId, target: PlayerId) -> bool {
    let Some(w) = watch(zone, target) else {
        return false;
    };
    if w.players.contains(&watcher) {
        return false;
    }
    w.players.push(watcher);
    let first = w.count() == 1;
    if first {
        toggle(zone, target, true);
    }
    true
}

/// Stops watching `target`.
pub fn remove_watch(zone: &mut Zone, watcher: PlayerId, target: PlayerId) {
    let Some(w) = watch(zone, target) else { return };
    if w.count() == 0 {
        return;
    }
    w.players.retain(|&x| x != watcher);
    let last = w.count() == 0;
    if last {
        toggle(zone, target, false);
    }
}

/// Drops every watch a player holds, and with `and_theirs` the watches
/// others hold on them (ASSS `ClearWatch`).
pub fn clear_watch(zone: &mut Zone, p: PlayerId, and_theirs: bool) {
    for target in zone.players.ids() {
        remove_watch(zone, p, target);
    }
    if and_theirs && let Some(w) = watch(zone, p) {
        w.players.clear();
    }
}

/// A module starts or stops watching a player (ASSS `ModuleWatch`). These
/// are counted apart from players' watches.
pub fn module_watch(zone: &mut Zone, p: PlayerId, on: bool) {
    let Some(w) = watch(zone, p) else { return };
    let before = w.count();
    if on {
        w.modules = w.modules.saturating_add(1);
    } else {
        w.modules = w.modules.saturating_sub(1);
    }
    let after = w.count();
    if before == 0 && after > 0 {
        toggle(zone, p, true);
    } else if before > 0 && after == 0 {
        toggle(zone, p, false);
    }
}

fn on_damage(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    let Some(player) = zone.players.get(p) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (player.arena, player.status) else {
        return;
    };
    let Ok(dmg) = C2SDamage::decode(pkt) else {
        tracing::warn!(target: "watchdamage", pid = p.0, len = pkt.len(), "bad damage packet");
        return;
    };
    let (watchers, modules) = match zone.players.get(p).and_then(|x| x.ext.get::<Watch>()) {
        Some(w) => (w.players.clone(), w.modules),
        None => return,
    };
    if !watchers.is_empty() {
        let mut buf = Vec::with_capacity(7 + dmg.entries.len() * 16);
        S2CDamage {
            pid: p.0 as i16,
            time: dmg.time,
            entries: dmg.entries.clone(),
        }
        .encode(&mut buf);
        zone.send_many(watchers, &buf, SendFlags::RELIABLE);
    }
    // these come in bulk, so the event is only for modules that asked
    if modules > 0 {
        zone.fire(&PlayerDamage {
            arena,
            player: p,
            time: dmg.time,
            entries: dmg.entries,
        });
    }
}

fn c_watchdamage(zone: &mut Zone, inv: &Invocation<'_>) {
    let me = inv.sender;
    let arg = inv.params.trim();
    match inv.target {
        Target::Arena(_) | Target::None => {
            // only "?watchdamage 0" means anything in public, as in ASSS
            if arg == "0" {
                clear_watch(zone, me, false);
                message(zone, me, "All damage watching turned off");
            }
        }
        Target::Player(t) => {
            let name = zone
                .players
                .get(t)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            if zone.players.get(t).map(|p| p.kind) != Some(ClientKind::Continuum) {
                message(
                    zone,
                    me,
                    &format!("watchdamage requires {name} to use Continuum"),
                );
                return;
            }
            let on = match arg {
                "0" => false,
                "1" => true,
                // no argument: turn it the other way
                _ => add_watch(zone, me, t),
            };
            if on {
                if arg == "1" {
                    add_watch(zone, me, t);
                }
                message(zone, me, &format!("Damage watching on {name} turned on"));
            } else {
                remove_watch(zone, me, t);
                message(zone, me, &format!("Damage watching on {name} turned off"));
            }
        }
        Target::Freq(..) => {}
    }
}

fn c_watchwatchdamage(zone: &mut Zone, inv: &Invocation<'_>) {
    let mut by_watcher: std::collections::HashMap<PlayerId, usize> = Default::default();
    let (mut reporting, mut to_modules) = (0usize, 0usize);
    for id in zone.players.ids() {
        let Some(w) = zone.players.get(id).and_then(|x| x.ext.get::<Watch>()) else {
            continue;
        };
        if w.modules > 0 {
            to_modules += 1;
        }
        if w.count() > 0 {
            reporting += 1;
        }
        for &watcher in &w.players {
            *by_watcher.entry(watcher).or_default() += 1;
        }
    }
    message(
        zone,
        inv.sender,
        &format!("wwd: total players reporting damage: {reporting}"),
    );
    message(
        zone,
        inv.sender,
        &format!("wwd: players reporting damage to modules: {to_modules}"),
    );
    let mut lines: Vec<(String, usize)> = by_watcher
        .into_iter()
        .filter_map(|(p, n)| zone.players.get(p).map(|x| (x.name.clone(), n)))
        .collect();
    lines.sort();
    for (name, n) in lines {
        message(
            zone,
            inv.sender,
            &format!("wwd: {name} is watching damage on {n} players"),
        );
    }
}

/// The `watchdamage` module.
pub struct WatchDamageModule;

impl Module for WatchDamageModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::DAMAGE, on_damage);
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            // leaving the arena ends both their watches and everyone's on them
            if matches!(ev.action, Action::LeaveArena | Action::Disconnect) {
                clear_watch(z, ev.player, true);
            }
        });
        zone.add_command(
            me,
            "watchdamage",
            None,
            Some(
                "Module: watchdamage\nTargets: player, none\nArgs: [0 or 1]\nTurns damage watching on and off. Aimed at a player, 1 turns it on, 0 turns it off and no argument turns it over. Typed in public, only ?watchdamage 0 does anything: it stops watching everyone.\n",
            ),
            c_watchdamage,
        );
        zone.add_command(
            me,
            "watchwatchdamage",
            None,
            Some(
                "Module: watchdamage\nTargets: none\nArgs: none\nSays how many players are reporting damage and who is watching whom.\n",
            ),
            c_watchwatchdamage,
        );
        Ok(())
    }
}
