//! `arenaman`: arena creation, entry, exit and destruction (ASSS
//! `arenaman.c`).
//!
//! Arenas are created on demand when a player asks for one, go through the
//! [`ArenaStatus`] states (config load, module attach, create callbacks,
//! "holds" that let modules delay a step), and are reaped when empty. On
//! entry the player receives, in ASSS order: their id, the settings, every
//! other player, their own player record, the map file name, a brick reset
//! and the "entering arena" marker.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena};
use oss_proto::common::PlayerData;
use oss_proto::s2c::{EnteringArena, PlayerLeaving};
use oss_proto::wire::FixedStr;

use crate::arena::{Arena, ArenaId, ArenaStatus};
use crate::config::to_latin1;
use crate::module::{self, Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, Player, PlayerId, PlayerStatus, Ship};
use crate::registry::{Event, Owner};
use crate::zone::Zone;

/// Default spectator frequency (`Team:SpectatorFrequency`).
pub const DEFAULT_SPEC_FREQ: u16 = 8025;

/// What is happening to an arena (ASSS `AA_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaActionKind {
    /// Very early: config loaded, nothing attached yet.
    PreCreate,
    /// Modules attached; set up arena state.
    Create,
    /// The arena's config changed on disk.
    ConfChanged,
    /// Being destroyed; modules still attached.
    Destroy,
    /// Destroyed; modules detached.
    PostDestroy,
}

/// ASSS `CB_ARENAACTION`.
#[derive(Debug, Clone, Copy)]
pub struct ArenaAction {
    /// The arena.
    pub arena: ArenaId,
    /// What is happening.
    pub action: ArenaActionKind,
}

impl Event for ArenaAction {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// Sends a player the arena's map (and LVZ) file names. Provided by the map
/// module.
pub trait MapFilename: Send + Sync {
    /// Queue S2C `0x29` for the player's arena.
    fn send_map_filename(&self, zone: &mut Zone, player: PlayerId);
}

/// Sends the settings packet. Provided by the client-settings module.
pub trait SendSettings: Send + Sync {
    /// Queue the player's arena settings.
    fn send_settings(&self, zone: &mut Zone, player: PlayerId);
}

/// Marks a player who has been sent the arena response for this arena.
struct Responded(ArenaId);

#[derive(Default)]
struct ArenaData {
    resurrect: bool,
    spec_freq: u16,
}

/// The arena's spectator frequency.
pub fn spec_freq(arena: &Arena) -> u16 {
    arena
        .ext
        .get::<ArenaData>()
        .map_or(DEFAULT_SPEC_FREQ, |d| d.spec_freq)
}

/// Delays the arena's current lifecycle step until [`unhold`] (ASSS `Hold`).
pub fn hold(zone: &mut Zone, arena: ArenaId) {
    if let Some(a) = zone.arenas.get_mut(arena) {
        if matches!(
            a.status,
            ArenaStatus::WaitHolds0 | ArenaStatus::WaitHolds1 | ArenaStatus::WaitHolds2
        ) {
            a.holds += 1;
        } else {
            tracing::warn!(target: "arenaman", arena = %a.name, status = ?a.status, "hold from wrong state");
        }
    }
}

/// Releases a [`hold`].
pub fn unhold(zone: &mut Zone, arena: ArenaId) {
    if let Some(a) = zone.arenas.get_mut(arena) {
        a.holds = a.holds.saturating_sub(1);
    }
}

fn fire(zone: &mut Zone, arena: ArenaId, action: ArenaActionKind) {
    zone.fire(&ArenaAction { arena, action });
}

fn do_attach(zone: &mut Zone, arena: ArenaId) {
    let list = zone
        .arenas
        .get(arena)
        .and_then(|a| a.cfg.get("Modules", "AttachModules").map(str::to_owned))
        .unwrap_or_default();
    for name in list
        .split([' ', '\t', ':', ';', ','])
        .filter(|s| !s.is_empty())
    {
        if let Err(e) = module::attach(zone, name, arena) {
            tracing::warn!(target: "arenaman", arena = arena.0, module = name, "can't attach: {e}");
        }
    }
}

fn process_arena_states(zone: &mut Zone) {
    for id in zone.arenas.ids() {
        let Some(a) = zone.arenas.get_mut(id) else {
            continue;
        };
        let holds = a.holds;
        let mut status = a.status;
        match status {
            ArenaStatus::WaitHolds0 if holds == 0 => status = ArenaStatus::DoInit1,
            ArenaStatus::WaitHolds1 if holds == 0 => status = ArenaStatus::DoInit2,
            ArenaStatus::WaitHolds2 if holds == 0 => status = ArenaStatus::DoDestroy2,
            _ => {}
        }
        a.status = status;
        match status {
            ArenaStatus::DoInit0 => {
                let base = a.basename.clone();
                let cfg = match zone.cfg.open_arena(&base) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(target: "arenaman", arena = %base, "{e}; using empty config");
                        std::sync::Arc::new(crate::config::ConfigFile::default())
                    }
                };
                let spec = u16::try_from(cfg.get_int(
                    "Team",
                    "SpectatorFrequency",
                    i32::from(DEFAULT_SPEC_FREQ),
                ))
                .unwrap_or(DEFAULT_SPEC_FREQ);
                if let Some(a) = zone.arenas.get_mut(id) {
                    a.cfg = cfg;
                    a.ext.get_or_default::<ArenaData>().spec_freq = spec;
                    a.status = ArenaStatus::WaitHolds0;
                }
                fire(zone, id, ArenaActionKind::PreCreate);
            }
            ArenaStatus::DoInit1 => {
                do_attach(zone, id);
                if let Some(a) = zone.arenas.get_mut(id) {
                    a.status = ArenaStatus::WaitHolds1;
                }
                fire(zone, id, ArenaActionKind::Create);
            }
            ArenaStatus::DoInit2 => {
                // no arena persistence yet: straight to running
                a.status = ArenaStatus::Running;
                tracing::info!(target: "arenaman", arena = %a.name, "arena running");
            }
            ArenaStatus::DoWriteData => {
                let occupied = zone.players.iter().any(|p| p.arena == Some(id));
                if let Some(a) = zone.arenas.get_mut(id) {
                    a.status = if occupied {
                        ArenaStatus::Running
                    } else {
                        ArenaStatus::DoDestroy1
                    };
                }
            }
            ArenaStatus::DoDestroy1 => {
                a.status = ArenaStatus::WaitHolds2;
                fire(zone, id, ArenaActionKind::Destroy);
            }
            ArenaStatus::DoDestroy2 => {
                module::detach_all(zone, id);
                fire(zone, id, ArenaActionKind::PostDestroy);
                let resurrect = zone
                    .arenas
                    .get_mut(id)
                    .map(|a| std::mem::take(&mut a.ext.get_or_default::<ArenaData>().resurrect))
                    .unwrap_or(false);
                if resurrect {
                    if let Some(a) = zone.arenas.get_mut(id) {
                        a.ext = Default::default();
                        a.status = ArenaStatus::DoInit0;
                    }
                } else {
                    zone.reg.forget_arena(id);
                    if let Some(a) = zone.arenas.remove(id) {
                        tracing::info!(target: "arenaman", arena = %a.name, "arena destroyed");
                    }
                }
            }
            _ => {}
        }
    }
}

fn reap_arenas(zone: &mut Zone) {
    for id in zone.arenas.ids() {
        let Some(a) = zone.arenas.get(id) else {
            continue;
        };
        if !matches!(a.status, ArenaStatus::Running | ArenaStatus::Closing) || a.keep_alive {
            continue;
        }
        let running = a.status == ArenaStatus::Running;
        let mut busy = false;
        let mut wanted = false;
        for p in zone.players.iter() {
            if p.arena == Some(id) || (p.new_arena == Some(id) && running) {
                busy = true;
                break;
            }
            if p.new_arena == Some(id) {
                wanted = true;
            }
        }
        if busy {
            continue;
        }
        if let Some(a) = zone.arenas.get_mut(id) {
            if wanted {
                a.ext.get_or_default::<ArenaData>().resurrect = true;
            }
            tracing::debug!(target: "arenaman", arena = %a.name, "arena being {}", if running { "destroyed" } else { "recycled" });
            a.status = ArenaStatus::DoWriteData;
        }
    }
}

/// Re-reads changed config files and tells running arenas whose config
/// changed (ASSS `CheckModifiedFiles` + `AA_CONFCHANGED`).
fn check_config(zone: &mut Zone) {
    for (old, new) in zone.cfg.reload_changed() {
        let affected: Vec<ArenaId> = zone
            .arenas
            .iter()
            .filter(|a| std::sync::Arc::ptr_eq(&a.cfg, &old))
            .map(|a| a.id)
            .collect();
        for id in affected {
            let running = match zone.arenas.get_mut(id) {
                Some(a) => {
                    a.cfg = new.clone();
                    a.status == ArenaStatus::Running
                }
                None => false,
            };
            if running {
                fire(zone, id, ArenaActionKind::ConfChanged);
            }
        }
    }
}

/// ASSS arena-name cleaning: at most 15 characters, `#` allowed first,
/// anything else not alphanumeric becomes `x`, lowercased; empty → `x`.
pub fn clean_arena_name(req: &str) -> String {
    let mut out: String = req
        .chars()
        .take(15)
        .enumerate()
        .map(|(i, c)| match c {
            '#' if i == 0 => '#',
            c if c.is_ascii_alphanumeric() => c.to_ascii_lowercase(),
            _ => 'x',
        })
        .collect();
    if out.is_empty() {
        out.push('x');
    }
    out
}

fn find_or_create(zone: &mut Zone, name: &str) -> ArenaId {
    match zone.arenas.find(name) {
        Some(id) => {
            if let Some(a) = zone.arenas.get_mut(id)
                && matches!(
                    a.status,
                    ArenaStatus::Closing
                        | ArenaStatus::DoWriteData
                        | ArenaStatus::WaitSync2
                        | ArenaStatus::DoDestroy1
                        | ArenaStatus::WaitHolds2
                        | ArenaStatus::DoDestroy2
                )
            {
                a.ext.get_or_default::<ArenaData>().resurrect = true;
            }
            id
        }
        None => {
            tracing::debug!(target: "arenaman", arena = name, "creating arena");
            zone.arenas.create(name)
        }
    }
}

/// Where a player wants to go and with what client options.
#[derive(Debug, Clone)]
pub struct GoRequest {
    /// Arena name (cleaned by [`clean_arena_name`]).
    pub name: String,
    /// Requested ship.
    pub ship: Ship,
    /// Screen size.
    pub resolution: (i16, i16),
    /// Wants optional LVZ.
    pub want_all_lvz: bool,
    /// Accepts voice messages.
    pub accept_audio: bool,
    /// Obscenity filter on.
    pub obscenity_filter: bool,
}

/// ASSS `complete_go`: leaves the current arena (if any) and queues entry
/// into `req.name`, creating the arena if needed. The core moves the player
/// in once the arena is running.
pub fn go(zone: &mut Zone, player: PlayerId, req: GoRequest) {
    let Some(st) = zone.players.get(player).map(|p| p.status) else {
        return;
    };
    if !matches!(
        st,
        PlayerStatus::LoggedIn | PlayerStatus::Playing | PlayerStatus::LeavingArena
    ) {
        tracing::warn!(target: "arenaman", pid = player.0, ?st, "arena request from bad status");
        return;
    }
    let name = clean_arena_name(&req.name);
    if zone.players.get(player).and_then(|p| p.arena).is_some() {
        leave_arena(zone, player);
    }
    let arena = find_or_create(zone, &name);
    if let Some(p) = zone.players.get_mut(player) {
        p.new_arena = Some(arena);
        p.ship = req.ship;
        p.resolution = req.resolution;
        p.want_all_lvz = req.want_all_lvz;
        p.accept_audio = req.accept_audio;
        p.obscenity_filter = req.obscenity_filter;
    }
}

/// ASSS `LeaveArena`.
pub fn leave_arena(zone: &mut Zone, player: PlayerId) {
    let Some(p) = zone.players.get(player) else {
        return;
    };
    let Some(arena) = p.arena else { return };
    let notify = match p.status {
        PlayerStatus::LoggedIn | PlayerStatus::DoFreqAndArenaSync => {
            set_status(zone, player, PlayerStatus::LoggedIn);
            false
        }
        PlayerStatus::WaitArenaSync1 => {
            super::core::leave_arena_when_done_waiting(zone, player);
            false
        }
        PlayerStatus::ArenaRespAndCbs => {
            set_status(zone, player, PlayerStatus::DoArenaSync2);
            false
        }
        PlayerStatus::Playing => {
            set_status(zone, player, PlayerStatus::LeavingArena);
            true
        }
        PlayerStatus::LeavingArena
        | PlayerStatus::DoArenaSync2
        | PlayerStatus::WaitArenaSync2
        | PlayerStatus::LeavingZone
        | PlayerStatus::WaitGlobalSync2 => false,
        st => {
            tracing::error!(target: "arenaman", pid = player.0, ?st, "player has an arena, but in bad state");
            true
        }
    };
    if let Some(p) = zone.players.get_mut(player) {
        p.ext.remove::<Responded>();
    }
    if notify {
        let mut buf = Vec::with_capacity(3);
        PlayerLeaving {
            pid: player.0 as i16,
        }
        .encode(&mut buf);
        zone.send_to_arena(arena, Some(player), &buf, SendFlags::RELIABLE);
        tracing::info!(target: "arenaman", pid = player.0, arena = arena.0, "leaving arena");
    }
}

fn set_status(zone: &mut Zone, p: PlayerId, s: PlayerStatus) {
    if let Some(pl) = zone.players.get_mut(p) {
        pl.status = s;
    }
}

/// The S2C `0x03` record describing `p`.
pub fn player_data(p: &Player) -> PlayerData {
    PlayerData {
        ship: p.ship.0 as i8,
        accept_audio: u8::from(p.accept_audio),
        name: FixedStr::new(&to_latin1(&p.name)),
        squad: FixedStr::new(&to_latin1(&p.squad)),
        kill_points: p.score.kill_points,
        flag_points: p.score.flag_points,
        pid: p.id.0 as i16,
        freq: p.freq as i16,
        wins: p.score.wins,
        losses: p.score.losses,
        attached_to: p.attached.map_or(-1, |h| h.0 as i16),
        flags_carried: p.flags_carried,
        misc_bits: super::koth::crown_bit(p),
    }
}

fn enter_bytes(p: &Player) -> Vec<u8> {
    let mut v = Vec::with_capacity(PlayerData::LEN);
    player_data(p).write(&mut v);
    v
}

/// ASSS `SendArenaResponse`.
pub fn send_arena_response(zone: &mut Zone, player: PlayerId) {
    let Some(p) = zone.players.get(player) else {
        return;
    };
    let Some(arena) = p.arena else {
        tracing::warn!(target: "arenaman", pid = player.0, "bad arena in send_arena_response");
        return;
    };
    let standard = matches!(p.kind, ClientKind::Vie | ClientKind::Continuum);
    let me = enter_bytes(p);
    tracing::info!(target: "arenaman", pid = player.0, name = %p.name, arena = arena.0, "entering arena");
    if standard {
        let mut buf = Vec::with_capacity(4);
        oss_proto::s2c::PlayerId {
            pid: player.0 as i16,
            extra: None,
        }
        .encode(&mut buf);
        zone.send(player, &buf, SendFlags::RELIABLE);
        if let Some(cs) = zone.iface_in::<dyn SendSettings>(arena) {
            cs.send_settings(zone, player);
        }
    }
    let others: Vec<(PlayerId, Vec<u8>)> = zone
        .players
        .in_arena(arena)
        .filter(|o| o.id != player)
        // players entering in the same tick are already Playing; only pair up
        // with those that got their own response, so nobody hears of anyone
        // twice (ASSS sends duplicates in this case)
        .filter(|o| o.ext.get::<Responded>().is_some_and(|r| r.0 == arena))
        .map(|o| (o.id, enter_bytes(o)))
        .collect();
    if let Some(p) = zone.players.get_mut(player) {
        p.ext.insert(Responded(arena));
    }
    for (oid, theirs) in &others {
        zone.send(*oid, &me, SendFlags::RELIABLE);
        zone.send(player, theirs, SendFlags::RELIABLE);
    }
    if standard {
        zone.send(player, &me, SendFlags::RELIABLE);
        if let Some(map) = zone.iface_in::<dyn MapFilename>(arena) {
            map.send_map_filename(zone, player);
        }
        zone.send(player, &[oss_proto::s2c::kind::BRICK], SendFlags::RELIABLE);
        let mut buf = Vec::with_capacity(1);
        EnteringArena {}.encode(&mut buf);
        zone.send(player, &buf, SendFlags::RELIABLE);
    }
}

fn has_cap_go(zone: &mut Zone, player: PlayerId) -> bool {
    match zone.iface::<dyn super::capman::Capabilities>() {
        Some(c) => c.has_capability(zone, player, "cmd_go"),
        None => true,
    }
}

fn on_go_arena(zone: &mut Zone, player: PlayerId, pkt: &[u8]) {
    if pkt.len() != 26 && pkt.len() != 27 {
        tracing::warn!(target: "arenaman", pid = player.0, len = pkt.len(), "bad arena packet length");
        return;
    }
    let Ok(go_pkt) = GoArena::decode(pkt) else {
        return;
    };
    if go_pkt.ship > 8 {
        tracing::warn!(target: "arenaman", pid = player.0, "bad ship type in arena request");
        return;
    }
    let name = match go_pkt.arena_type {
        -3 => {
            if !has_cap_go(zone, player) {
                return;
            }
            String::from_utf8_lossy(go_pkt.arena.as_bytes()).into_owned()
        }
        -2 | -1 => "0".to_owned(),
        n if n >= 0 => {
            if !has_cap_go(zone, player) {
                return;
            }
            n.to_string()
        }
        _ => {
            tracing::warn!(target: "arenaman", pid = player.0, "bad arena type in arena request");
            return;
        }
    };
    let force_filter = zone.cfg.global().get_int("Chat", "ForceFilter", 0) != 0;
    go(
        zone,
        player,
        GoRequest {
            name,
            ship: Ship(go_pkt.ship),
            resolution: (go_pkt.x_res, go_pkt.y_res),
            want_all_lvz: go_pkt.optional_graphics.unwrap_or(0) != 0,
            accept_audio: go_pkt.allow_audio != 0,
            obscenity_filter: go_pkt.obscenity_filter != 0 || force_filter,
        },
    );
}

fn on_leaving(zone: &mut Zone, player: PlayerId, pkt: &[u8]) {
    if pkt.len() != 1 {
        tracing::warn!(target: "arenaman", pid = player.0, len = pkt.len(), "bad arena leaving packet");
        return;
    }
    leave_arena(zone, player);
}

/// The `arenaman` module.
pub struct ArenaManModule;

impl Module for ArenaManModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::GO_ARENA, on_go_arena);
        zone.on_packet(me, c2s::kind::LEAVE_ARENA, on_leaving);
        zone.set_timer(me, 1, 1, |z| {
            process_arena_states(z);
            true
        });
        zone.set_timer(me, 170, 170, |z| {
            reap_arenas(z);
            true
        });
        let interval = u64::try_from(zone.cfg.global().get_int(
            "Config",
            "CheckModifiedFilesInterval",
            1500,
        ))
        .unwrap_or(1500);
        if interval > 0 {
            zone.set_timer(me, interval, interval, |z| {
                check_config(z);
                true
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_names_are_cleaned() {
        assert_eq!(clean_arena_name("Turf"), "turf");
        assert_eq!(clean_arena_name("#Priv-1"), "#privx1");
        assert_eq!(clean_arena_name("a#b"), "axb");
        assert_eq!(clean_arena_name(""), "x");
        assert_eq!(clean_arena_name("abcdefghijklmnopq"), "abcdefghijklmno");
    }
}
