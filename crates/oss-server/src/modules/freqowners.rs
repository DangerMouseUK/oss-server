//! `freqowners`: owners of private frequencies (ASSS `freqowners.c`).
//!
//! The first player on an empty private frequency (at or above
//! `Team:PrivFreqStart`, when `Team:AllowFreqOwners` is on) owns it. Owners
//! can `?freqkick` non-owners off the frequency (after three seconds) and
//! share ownership with `?giveowner`; `?takeownership` claims a frequency
//! whose owners have all left. Ownership ends on any frequency change or
//! arena change.
//!
//! One deliberate difference from ASSS: there a ship change on the same
//! frequency counts as a frequency change, so an owner who switches ship
//! while others are on the frequency loses ownership, and a kicked player can
//! dodge a pending `?freqkick` by switching ship. Here a ship change that
//! keeps the frequency changes nothing.

use crate::arena::ArenaId;
use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, Ship};
use crate::registry::{Owner, Scope};
use crate::timers::TimerId;
use crate::zone::Zone;

use super::chat::{message, wrapped_message};
use super::core::{Action, PlayerAction};
use super::game::ShipFreqChange;

/// Ticks between `?freqkick` and the kick (ASSS `KICKDELAY`).
pub const KICK_DELAY: u64 = 300;

#[derive(Debug, Default)]
struct FreqOwner {
    owns: bool,
    kick: Option<TimerId>,
}

fn state(zone: &mut Zone, p: PlayerId) -> Option<&mut FreqOwner> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<FreqOwner>())
}

/// True if `p` owns their frequency.
pub fn owns_freq(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<FreqOwner>())
        .is_some_and(|o| o.owns)
}

fn set_owner(zone: &mut Zone, p: PlayerId, owns: bool) {
    if let Some(s) = state(zone, p) {
        s.owns = owns;
    }
}

fn reset(zone: &mut Zone, p: PlayerId) {
    if let Some(s) = state(zone, p) {
        s.owns = false;
        if let Some(t) = s.kick.take() {
            zone.timers.cancel(t);
        }
    }
}

fn owner_allowed(zone: &Zone, arena: ArenaId, freq: u16) -> bool {
    let Some(a) = zone.arenas.get(arena) else {
        return false;
    };
    a.cfg.get_int("Team", "AllowFreqOwners", 1) != 0
        && i32::from(freq) >= a.cfg.get_int("Team", "PrivFreqStart", 100)
        && freq != super::arenaman::spec_freq(a)
}

/// Players on `freq` other than `excl`, and the names of its owners.
fn count_freq(zone: &Zone, arena: ArenaId, freq: u16, excl: PlayerId) -> (usize, Vec<String>) {
    let mut total = 0;
    let mut owners = Vec::new();
    for p in zone.players.in_arena(arena) {
        if p.freq == freq && p.id != excl {
            total += 1;
            if p.ext.get::<FreqOwner>().is_some_and(|o| o.owns) {
                owners.push(p.name.clone());
            }
        }
    }
    (total, owners)
}

/// A player on the sender's frequency, in the sender's arena.
fn teammate(zone: &Zone, inv: &Invocation<'_>) -> Option<PlayerId> {
    let Target::Player(t) = inv.target else {
        return None;
    };
    let me = zone.players.get(inv.sender)?;
    let them = zone.players.get(t)?;
    (them.arena.is_some() && them.arena == me.arena && them.freq == me.freq).then_some(t)
}

fn name(zone: &Zone, p: PlayerId) -> String {
    zone.players
        .get(p)
        .map(|x| x.name.clone())
        .unwrap_or_default()
}

fn c_giveowner(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    if !owns_freq(zone, p) {
        message(zone, p, "You are not the owner of your freq.");
    } else if let Some(t) = teammate(zone, inv) {
        if owns_freq(zone, t) {
            message(zone, p, "That player is already an owner of your freq.");
        } else {
            set_owner(zone, t, true);
            let (pn, tn) = (name(zone, p), name(zone, t));
            message(zone, p, &format!("You have granted ownership to {tn}"));
            message(
                zone,
                t,
                &format!("{pn} has granted you ownership of your freq."),
            );
        }
    } else {
        message(
            zone,
            p,
            "You must send this command to a player on your freq.",
        );
    }
}

fn c_takeownership(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    let Some((arena, freq)) = zone
        .players
        .get(p)
        .and_then(|x| x.arena.map(|a| (a, x.freq)))
    else {
        return;
    };
    if !owner_allowed(zone, arena, freq) {
        message(zone, p, "Your freq can not have an owner.");
    } else if owns_freq(zone, p) {
        message(zone, p, "You are already an owner of your freq.");
    } else {
        let (_, owners) = count_freq(zone, arena, freq, p);
        if owners.is_empty() {
            set_owner(zone, p, true);
            message(zone, p, "You are now the owner of your freq.");
        } else {
            message(zone, p, "Your freq already has one or more owners:");
            wrapped_message(zone, p, &owners.join(", "));
        }
    }
}

fn c_freqkick(zone: &mut Zone, inv: &Invocation<'_>, owner: Owner) {
    let p = inv.sender;
    if !owns_freq(zone, p) {
        message(zone, p, "You are not the owner of your freq.");
        return;
    }
    let Some(t) = teammate(zone, inv) else {
        message(
            zone,
            p,
            "You must send this command to a player on your freq.",
        );
        return;
    };
    if owns_freq(zone, t) {
        message(zone, p, "You can not kick other freq owners off your freq.");
        return;
    }
    let tn = name(zone, t);
    message(zone, p, &format!("{tn} will be kicked off momentarily."));
    let id = zone.set_timer(owner, KICK_DELAY, 0, move |z| {
        if let Some(s) = state(z, t) {
            s.kick = None;
        }
        let Some(a) = z.players.get(t).and_then(|x| x.arena) else {
            return false;
        };
        message(z, t, "You have been kicked off your freq.");
        let sf = z.arenas.get(a).map_or(8025, super::arenaman::spec_freq);
        super::game::set_ship_and_freq(z, t, Ship::SPEC, sf);
        false
    });
    if let Some(s) = state(zone, t)
        && let Some(old) = s.kick.replace(id)
    {
        zone.timers.cancel(old);
    }
}

fn on_freq_change(zone: &mut Zone, ev: &ShipFreqChange) {
    let p = ev.player;
    if ev.freq == ev.old_freq {
        // a ship change on the same frequency (see the module docs)
        return;
    }
    reset(zone, p);
    if !owner_allowed(zone, ev.arena, ev.freq) {
        return;
    }
    let (total, owners) = count_freq(zone, ev.arena, ev.freq, p);
    if total == 0 {
        set_owner(zone, p, true);
        message(
            zone,
            p,
            &format!(
                "You are the now the owner of freq {}. You can kick people off your freq by \
                 sending them the private message \"?freqkick\", and you can share your \
                 ownership by sending people \"?giveowner\".",
                ev.freq
            ),
        );
    } else if !owners.is_empty() {
        message(
            zone,
            p,
            "This freq has an owner. Be aware that you can be kicked off any time.",
        );
        message(
            zone,
            p,
            "If the owner leaves, you can use ?takeownership to become the owner.",
        );
    }
}

/// The `freqowners` module.
pub struct FreqOwnersModule;

impl Module for FreqOwnersModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if matches!(ev.action, Action::EnterArena | Action::LeaveArena) {
                reset(z, ev.player);
            }
        });
        zone.on::<ShipFreqChange>(me, Scope::Global, on_freq_change);
        zone.add_command(
            me,
            "giveowner",
            None,
            Some(
                "Module: freqowners\nTargets: player\nArgs: none\n\
                 Allows you to share freq ownership with another player on your current\n\
                 private freq. You can't remove ownership once you give it out, but you\n\
                 are safe from being kicked off yourself, as long as you have ownership.\n",
            ),
            c_giveowner,
        );
        zone.add_command(
            me,
            "freqkick",
            None,
            Some(
                "Module: freqowners\nTargets: player\nArgs: none\n\
                 Kicks the player off of your freq. The player must be on your freq and\n\
                 must not be an owner himself. The player giving the command, of course,\n\
                 must be an owner.\n",
            ),
            move |z, inv| c_freqkick(z, inv, me),
        );
        zone.add_command(
            me,
            "takeownership",
            None,
            Some(
                "Module: freqowners\nTargets: none\nArgs: none\n\
                 Makes you become owner of your freq, if your freq doesn't\n\
                 have an owner already.\n",
            ),
            c_takeownership,
        );
        Ok(())
    }
}
