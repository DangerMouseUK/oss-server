//! `koth`: King of the Hill (attach to arenas; ASSS `koth.c`).
//!
//! When at least `King:MinPlaying` players are in ships, everyone gets a
//! crown with `King:ExpireTime` ticks on it (S2C `0x2C`). Killing someone
//! resets your timer; dying more than `King:DeathCount` times, or letting the
//! timer run out (C2S `0x1E`), loses the crown. Players without a crown win
//! it back after `King:CrownRecoverKills` kills of crowned players. When the
//! last crowns go at once, their holders split 1000 points (flag points) and
//! a new round starts. `?resetkoth` restarts the round.

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::s2c::Koth;

use crate::arena::ArenaId;
use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, Player, PlayerId};
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::chat::message;
use super::core::{Action, PlayerAction};
use super::game::{Kill, ShipFreqChange};
use super::stats::{Stats, stat};

/// KOTH games won (ASSS stat code).
const KOTH_GAMES_WON: u16 = 400;

/// Why a crown changed hands (ASSS `KOTH_CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrownCause {
    /// A round started.
    GameStart,
    /// Left the game (arena, spectator).
    LeaveGame,
    /// Died too often.
    TooManyDeaths,
    /// Earned back with kills.
    Recovered,
    /// Timer ran out.
    Expired,
}

/// ASSS `CB_CROWNCHANGE`.
#[derive(Debug, Clone, Copy)]
pub struct CrownChange {
    /// Arena.
    pub arena: ArenaId,
    /// Player.
    pub player: PlayerId,
    /// Gained (true) or lost.
    pub gained: bool,
    /// Why.
    pub cause: CrownCause,
}

impl Event for CrownChange {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

#[derive(Clone, Copy, Default)]
struct KothSettings {
    death_count: i32,
    expire_time: u32,
    recover_kills: i32,
    min_playing: usize,
}

#[derive(Clone, Copy, Default)]
struct Crown {
    crown: bool,
    had_crown: bool,
    deaths: i32,
    crown_kills: i32,
}

fn settings(zone: &Zone, a: ArenaId) -> Option<KothSettings> {
    zone.arenas.get(a)?.ext.get::<KothSettings>().copied()
}

fn crown(zone: &mut Zone, p: PlayerId) -> Option<&mut Crown> {
    zone.players
        .get_mut(p)
        .map(|p| p.ext.get_or_default::<Crown>())
}

fn send_koth(zone: &mut Zone, to: &[PlayerId], action: u8, time: u32, pid: i16) {
    let mut buf = Vec::with_capacity(8);
    Koth { action, time, pid }.encode(&mut buf);
    zone.send_many(to.iter().copied(), &buf, SendFlags::RELIABLE);
}

fn start_koth(zone: &mut Zone, a: ArenaId) {
    let Some(s) = settings(zone, a) else { return };
    let members: Vec<(PlayerId, bool)> = zone
        .players
        .in_arena(a)
        .map(|p| (p.id, !p.ship.is_spec()))
        .collect();
    let mut set = Vec::new();
    for (pid, playing) in members {
        if let Some(c) = crown(zone, pid) {
            *c = Crown {
                crown: playing,
                had_crown: playing,
                deaths: 0,
                crown_kills: 0,
            };
        }
        if playing {
            set.push(pid);
        }
    }
    send_koth(zone, &set, 1, s.expire_time, -1);
    if let Some(c) = zone.iface::<dyn super::chat::Chat>() {
        c.send_arena_message(zone, a, "King of the Hill game starting");
    }
    for pid in set {
        zone.fire(&CrownChange {
            arena: a,
            player: pid,
            gained: true,
            cause: CrownCause::GameStart,
        });
    }
}

fn check_koth(zone: &mut Zone, a: ArenaId) -> bool {
    let Some(s) = settings(zone, a) else {
        return false;
    };
    let mut playing = 0;
    let mut crowns = 0;
    let mut had = Vec::new();
    for p in zone.players.in_arena(a) {
        if p.ship.is_spec() || !matches!(p.kind, ClientKind::Vie | ClientKind::Continuum) {
            continue;
        }
        playing += 1;
        let c = p.ext.get::<Crown>().copied().unwrap_or_default();
        if c.crown {
            crowns += 1;
        }
        if c.had_crown {
            had.push((p.id, p.name.clone()));
        }
    }
    if crowns == 0 && !had.is_empty() {
        let pts = 1000 / had.len() as i32;
        let st = zone.iface::<dyn Stats>();
        for (pid, name) in &had {
            if let Some(st) = &st {
                st.increment(zone, *pid, stat::FLAG_POINTS, pts);
                st.increment(zone, *pid, KOTH_GAMES_WON, 1);
            }
            if let Some(c) = zone.iface::<dyn super::chat::Chat>() {
                c.send_arena_message(
                    zone,
                    a,
                    &format!("King of the Hill: {name} awarded {pts} points"),
                );
            }
        }
        if let Some(st) = &st {
            st.send_updates(zone, None);
        }
        if playing >= s.min_playing {
            start_koth(zone, a);
        }
    } else if crowns == 0 && playing >= s.min_playing {
        start_koth(zone, a);
    }
    let ids: Vec<PlayerId> = zone
        .players
        .in_arena(a)
        .filter(|p| !p.ship.is_spec())
        .map(|p| p.id)
        .collect();
    for pid in ids {
        if let Some(c) = crown(zone, pid) {
            c.had_crown = c.crown;
        }
    }
    true
}

fn set_crown_time(zone: &mut Zone, p: PlayerId, time: u32) {
    if let Some(c) = crown(zone, p) {
        c.crown = true;
        c.had_crown = true;
    }
    send_koth(zone, &[p], 1, time, -1);
}

fn remove_crown(zone: &mut Zone, a: ArenaId, p: PlayerId) {
    if settings(zone, a).is_none_or(|s| s.expire_time == 0) {
        return;
    }
    if let Some(c) = crown(zone, p) {
        c.crown = false;
    }
    let to: Vec<PlayerId> = zone.players.in_arena(a).map(|x| x.id).collect();
    send_koth(zone, &to, 0, 0, p.0 as i16);
}

fn clear(zone: &mut Zone, a: ArenaId, p: PlayerId) {
    if let Some(c) = crown(zone, p) {
        c.crown = false;
        c.had_crown = false;
    }
    zone.fire(&CrownChange {
        arena: a,
        player: p,
        gained: false,
        cause: CrownCause::LeaveGame,
    });
}

fn on_kill(zone: &mut Zone, k: &Kill) {
    let Some(s) = settings(zone, k.arena) else {
        return;
    };
    let killed_crown = crown(zone, k.killed).is_some_and(|c| c.crown);
    if killed_crown {
        let too_many = crown(zone, k.killed).is_some_and(|c| {
            c.deaths += 1;
            c.deaths > s.death_count
        });
        if too_many {
            remove_crown(zone, k.arena, k.killed);
            zone.fire(&CrownChange {
                arena: k.arena,
                player: k.killed,
                gained: false,
                cause: CrownCause::TooManyDeaths,
            });
        }
    }
    if crown(zone, k.killer).is_some_and(|c| c.crown) {
        set_crown_time(zone, k.killer, s.expire_time);
    } else if killed_crown && s.recover_kills > 0 {
        let left = crown(zone, k.killer).map_or(0, |c| {
            c.crown_kills += 1;
            s.recover_kills - c.crown_kills
        });
        if left <= 0 {
            if let Some(c) = crown(zone, k.killer) {
                c.crown_kills = 0;
                c.deaths = 0;
            }
            set_crown_time(zone, k.killer, s.expire_time);
            message(zone, k.killer, "You earned back a crown");
            zone.fire(&CrownChange {
                arena: k.arena,
                player: k.killer,
                gained: true,
                cause: CrownCause::Recovered,
            });
        } else {
            let plural = if left == 1 { "" } else { "s" };
            message(
                zone,
                k.killer,
                &format!("{left} kill{plural} left to earn back a crown"),
            );
        }
    }
}

fn on_expired(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    let arena = zone.players.get(p).and_then(|x| x.arena);
    let (1, Some(a)) = (pkt.len(), arena) else {
        tracing::warn!(target: "koth", pid = p.0, len = pkt.len(), "bad KoTH expired packet");
        return;
    };
    if settings(zone, a).is_none() {
        return;
    }
    remove_crown(zone, a, p);
    zone.fire(&CrownChange {
        arena: a,
        player: p,
        gained: false,
        cause: CrownCause::Expired,
    });
}

fn c_resetkoth(zone: &mut Zone, inv: &Invocation<'_>) {
    if let Some(a) = inv.arena
        && settings(zone, a).is_some_and(|s| s.expire_time != 0)
    {
        start_koth(zone, a);
    }
}

/// Bit 0 of the player record's misc bits: has a crown.
pub fn crown_bit(p: &Player) -> u8 {
    u8::from(p.ext.get::<Crown>().is_some_and(|c| c.crown))
}

/// Whether a player currently has a crown.
pub fn has_crown(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<Crown>())
        .is_some_and(|c| c.crown)
}

/// The `koth` module (attach to arenas).
pub struct KothModule;

impl Module for KothModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::KOTH_EXPIRED, on_expired);
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        let Some(cfg) = zone.arenas.get(arena).map(|a| a.cfg.clone()) else {
            return Ok(());
        };
        let s = KothSettings {
            death_count: cfg.get_int("King", "DeathCount", 0),
            expire_time: u32::try_from(cfg.get_int("King", "ExpireTime", 18000)).unwrap_or(0),
            recover_kills: cfg.get_int("King", "CrownRecoverKills", 0),
            min_playing: usize::try_from(cfg.get_int("King", "MinPlaying", 3)).unwrap_or(3),
        };
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.insert(s);
        }
        zone.on::<Kill>(me, Scope::Arena(arena), on_kill);
        zone.on::<PlayerAction>(me, Scope::Arena(arena), |z, ev| {
            if let (Action::EnterArena | Action::LeaveArena, Some(a)) = (ev.action, ev.arena) {
                clear(z, a, ev.player);
            }
        });
        zone.on::<ShipFreqChange>(me, Scope::Arena(arena), |z, ev| {
            if ev.ship.is_spec() {
                clear(z, ev.arena, ev.player);
            }
        });
        zone.add_command(
            me,
            "resetkoth",
            Some(arena),
            Some("Module: koth\nTargets: none\nArgs: none\nStarts a new King of the Hill round.\n"),
            c_resetkoth,
        );
        zone.set_timer(me, 500, 500, move |z| {
            if z.arenas
                .get(arena)
                .is_none_or(|a| a.ext.get::<KothSettings>().is_none())
            {
                return false;
            }
            check_koth(z, arena)
        });
        Ok(())
    }

    fn detach(&self, zone: &mut Zone, _me: Owner, arena: ArenaId) {
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.remove::<KothSettings>();
        }
    }
}
