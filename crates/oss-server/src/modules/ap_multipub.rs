//! `ap_multipub`: spreads players over several public arenas (ASSS
//! `ap_multipub.c`).
//!
//! Without it a player who asks for "any public arena" always lands in the
//! same one. With it, `General:PublicArenas` lists the kinds of public arena
//! a zone has (`pub`, `twd`, …) and the server numbers them from 1: a player
//! goes into the first `<name><n>` with fewer than `General:DesiredPlaying`
//! (15) players in ships, and a new one is created when they are all busy.
//!
//! A player who connected through a listen socket with `Listen:ConnectAs`
//! set is placed among that virtual server's own arenas instead, as in ASSS.
//!
//! The module is inert until `General:PublicArenas` is set, so loading it in
//! a zone that hasn't configured it changes nothing.

use std::sync::Arc;

use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::ArenaPlace;

/// How many numbered arenas of each kind to consider (ASSS tries 9 passes).
const MAX_PASSES: u32 = 9;

/// ASSS copies each name, and then each `<name><pass>`, into a 20-byte
/// buffer, so both are cut to 19 characters. A name near that limit loses
/// its pass digit, and two names that differ only past the 19th character
/// become the same arena.
const MAX_NAME: usize = 19;

/// Cuts to [`MAX_NAME`] bytes without splitting a character.
fn truncate(s: &str) -> String {
    s.char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= MAX_NAME)
        .map(|(_, c)| c)
        .collect()
}

struct MultiPub;

/// Players in `name` who are in a ship, and whether the arena exists.
fn playing_in(zone: &Zone, name: &str) -> Option<usize> {
    let arena = zone.arenas.iter().find(|a| a.name == name)?;
    Some(
        zone.players
            .in_arena(arena.id)
            .filter(|p| p.status == PlayerStatus::Playing && !p.ship.is_spec())
            .count(),
    )
}

fn desired_playing(zone: &Zone, name: &str) -> i32 {
    zone.arenas
        .iter()
        .find(|a| a.name == name)
        .map_or(15, |a| a.cfg.get_int("General", "DesiredPlaying", 15))
}

impl ArenaPlace for MultiPub {
    fn place(&self, zone: &mut Zone, player: PlayerId) -> Option<String> {
        // a virtual server's players stay among its own arenas
        let names: Vec<String> = match zone.players.get(player).and_then(|p| p.connect_as.clone()) {
            Some(c) => vec![truncate(&c)],
            None => zone
                .cfg
                .global()
                .get("General", "PublicArenas")
                .unwrap_or("")
                .split([' ', ',', ':', ';'])
                .filter(|s| !s.is_empty())
                .map(truncate)
                .collect(),
        };
        if names.is_empty() {
            return None;
        }
        // the first arena that isn't full takes them; the first one that
        // doesn't exist yet is the fallback, so a busy zone grows
        let mut backup: Option<String> = None;
        for pass in 1..=MAX_PASSES {
            for name in &names {
                let candidate = truncate(&format!("{name}{pass}"));
                match playing_in(zone, &candidate) {
                    None => {
                        if backup.is_none() {
                            backup = Some(candidate);
                        }
                    }
                    Some(playing) => {
                        if (playing as i32) < desired_playing(zone, &candidate) {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
        backup
    }
}

/// The `ap_multipub` module.
pub struct ApMultiPubModule;

impl Module for ApMultiPubModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn ArenaPlace>(me, Scope::Global, Arc::new(MultiPub));
        Ok(())
    }
}
