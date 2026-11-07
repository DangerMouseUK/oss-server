//! `game`: position and weapon relay, ship and frequency changes, spectating
//! (the core of ASSS `game.c`).
//!
//! Position relay follows ASSS: a plain update goes out as S2C `0x28` to
//! players whose screen could show it, plus a distance-weighted random share
//! of radar updates within `Net:PositionExtraPixels`; weapons, bounties over
//! 255 and player ids over 255 use S2C `0x05` within the weapon's range
//! (`Net:BulletPixels`, `Net:WeaponPixels`). Mines, flashes, safe-zone entries
//! and a percentage of antiwarp updates go to everyone; spectators always get
//! the player they watch. Energy is appended per recipient according to
//! `Misc:SeeEnergy`/`Misc:SpecSeeEnergy`/`Misc:SpecSeeExtra`.
//!
//! Not yet ported: regions, weapon-ignore percentages, ship locks, the
//! ship-change flood limit, turrets and the "during change" ack wait.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::position::{C2SPosition, PositionTail, S2CPosition, S2CWeapon, status, weapon_type};
use oss_proto::s2c::{FreqChange, ShipChange};

use crate::arena::{ArenaId, ArenaStatus};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus, Ship};
use crate::prng::Prng;
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::arenaman::spec_freq;
use super::core::{Action, FreqManager, PlayerAction};

/// ASSS `ENERGY_SEE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeeEnergy {
    /// Nobody's energy.
    #[default]
    None,
    /// Everyone's.
    All,
    /// Own team only.
    Team,
    /// The player being spectated.
    Spec,
}

impl SeeEnergy {
    fn from_cfg(v: i32) -> Self {
        match v {
            1 => Self::All,
            2 => Self::Team,
            3 => Self::Spec,
            _ => Self::None,
        }
    }
}

/// ASSS `CB_SHIPCHANGE` (also raised for frequency-only changes).
#[derive(Debug, Clone, Copy)]
pub struct ShipFreqChange {
    /// The player.
    pub player: PlayerId,
    /// The arena.
    pub arena: ArenaId,
    /// New ship.
    pub ship: Ship,
    /// New frequency.
    pub freq: u16,
}

impl Event for ShipFreqChange {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

#[derive(Default)]
struct GameData {
    /// last accepted position packet: (x, y, time, status)
    x: i16,
    y: i16,
    time: u32,
    status: u8,
    have_pos: bool,
    speccing: Option<PlayerId>,
    see_nrg: SeeEnergy,
    see_nrg_spec: SeeEnergy,
    see_epd: bool,
}

#[derive(Clone, Copy)]
struct NetCfg {
    bullet_pix: i64,
    weapon_pix: i64,
    pos_pix: i64,
    anti_percent: u32,
}

fn net_cfg(zone: &Zone) -> NetCfg {
    let g = zone.cfg.global();
    NetCfg {
        bullet_pix: i64::from(g.get_int("Net", "BulletPixels", 1500)),
        weapon_pix: i64::from(g.get_int("Net", "WeaponPixels", 2000)),
        pos_pix: i64::from(g.get_int("Net", "PositionExtraPixels", 8000)),
        anti_percent: u32::try_from(g.get_int("Net", "AntiwarpSendPercent", 5)).unwrap_or(5),
    }
}

impl NetCfg {
    fn range(&self, weapon: u8) -> i64 {
        match weapon {
            weapon_type::BULLET | weapon_type::BOUNCE_BULLET => self.bullet_pix,
            weapon_type::THOR => 30000,
            _ => self.weapon_pix,
        }
    }
}

fn gd(zone: &mut Zone, p: PlayerId) -> Option<&mut GameData> {
    zone.players
        .get_mut(p)
        .map(|p| p.ext.get_or_default::<GameData>())
}

fn rand(zone: &mut Zone) -> &mut Prng {
    zone.res.get_or_default::<Prng>()
}

/// Which variant of the outgoing packet a recipient gets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    Plain,
    Energy,
    Extra,
}

fn on_position(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 22 && pkt.len() != 32 {
        tracing::warn!(target: "game", pid = pid.0, len = pkt.len(), "bad position packet length");
        return;
    }
    let now = zone.now();
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    if zone
        .arenas
        .get(arena)
        .is_none_or(|a| a.status != ArenaStatus::Running)
    {
        return;
    }
    let fake = p.kind == ClientKind::Fake;
    let (ship, freq) = (p.ship, p.freq);
    if !fake && pkt[..22].iter().fold(0u8, |a, &b| a ^ b) != 0 {
        tracing::warn!(target: "game", pid = pid.0, "bad position packet checksum");
        return;
    }
    let Ok(mut pos) = C2SPosition::decode(pkt) else {
        return;
    };
    if pos.x == -1 && pos.y == -1 {
        return;
    }
    let latency = now.wire().wrapping_sub(pos.time) as i32;
    let latency = latency.clamp(0, 255) as u8;
    let (old_time, old_status, had) = zone
        .players
        .get(pid)
        .and_then(|p| p.ext.get::<GameData>())
        .map_or((0, 0, false), |d| (d.time, d.status, d.have_pos));
    let is_newer = !had || (pos.time.wrapping_sub(old_time) as i32) > 0;

    if !ship.is_spec() {
        if !is_newer && !fake && pos.weapon.kind() == weapon_type::NONE {
            return;
        }
        relay(
            zone,
            pid,
            arena,
            freq,
            &mut pos,
            pkt.len(),
            latency,
            old_status,
        );
    }

    if is_newer || fake {
        if let Some(d) = gd(zone, pid) {
            d.x = pos.x;
            d.y = pos.y;
            d.time = pos.time;
            d.status = pos.status;
            d.have_pos = true;
        }
        if let Some(p) = zone.players.get_mut(pid) {
            if pos.x != 0 || pos.y != 0 {
                p.pos.x = pos.x;
                p.pos.y = pos.y;
            }
            p.pos.xspeed = pos.x_speed;
            p.pos.yspeed = pos.y_speed;
            p.pos.rotation = pos.rotation as u8;
            p.pos.bounty = pos.bounty;
            p.pos.status = pos.status;
            p.pos.energy = pos.energy as u16;
            p.pos.time = pos.time;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn relay(
    zone: &mut Zone,
    pid: PlayerId,
    arena: ArenaId,
    freq: u16,
    pos: &mut C2SPosition,
    len: usize,
    latency: u8,
    old_status: u8,
) {
    let cfg = net_cfg(zone);
    let wtype = pos.weapon.kind();
    let mut flags = SendFlags::DROPPABLE;
    flags.urgent = wtype != weapon_type::NONE;
    let send_wpn = wtype != weapon_type::NONE || pos.bounty & 0xFF00 != 0 || pid.0 & 0xFF00 != 0;
    let mut to_all =
        matches!(wtype, weapon_type::BOMB | weapon_type::PROX_BOMB) && pos.weapon.alternate();
    if wtype == weapon_type::NONE
        && pos.status & status::ANTIWARP != 0
        && rand(zone).chance(cfg.anti_percent, 100)
    {
        to_all = true;
    }
    if pos.status & status::SAFEZONE != 0 && old_status & status::SAFEZONE == 0 {
        to_all = true;
        flags = SendFlags::RELIABLE;
    }
    if pos.status & status::FLASH != 0 {
        to_all = true;
        flags = SendFlags::RELIABLE;
    }
    // one random draw per packet: radar updates go to recipients closer than
    // r * PositionExtraPixels
    let radar_cut = i64::from(rand(zone).below(1 << 16)) * cfg.pos_pix / (1 << 16);
    let range = if send_wpn { cfg.range(wtype) } else { 0 };
    let (x1, y1) = (i64::from(pos.x), i64::from(pos.y));

    let mut groups: [Vec<PlayerId>; 3] = Default::default();
    for i in zone.players.iter() {
        if i.status != PlayerStatus::Playing
            || i.arena != Some(arena)
            || i.id == pid
            || !matches!(i.kind, ClientKind::Vie | ClientKind::Continuum)
        {
            continue;
        }
        let d = i.ext.get::<GameData>();
        let (ix, iy) = d.map_or((0, 0), |d| (i64::from(d.x), i64::from(d.y)));
        let dist = (((x1 - ix).pow(2) + (y1 - iy).pow(2)) as f64).sqrt() as i64;
        let watching = d.is_some_and(|d| d.speccing == Some(pid));
        let radar = dist <= cfg.pos_pix && dist < radar_cut;
        let near = if send_wpn {
            dist <= range || (wtype == weapon_type::NONE && radar)
        } else {
            let res = i64::from(i.resolution.0) + i64::from(i.resolution.1);
            dist < res || radar
        };
        if !(near || to_all || watching) {
            continue;
        }
        let tail = match d {
            Some(d) if i.ship.is_spec() => {
                if d.see_epd && watching {
                    if len >= 32 { Tail::Extra } else { Tail::Energy }
                } else if d.see_nrg_spec == SeeEnergy::All
                    || (d.see_nrg_spec == SeeEnergy::Team && i.freq == freq)
                    || (d.see_nrg_spec == SeeEnergy::Spec && watching)
                {
                    Tail::Energy
                } else {
                    Tail::Plain
                }
            }
            Some(d)
                if d.see_nrg == SeeEnergy::All
                    || (d.see_nrg == SeeEnergy::Team && i.freq == freq) =>
            {
                Tail::Energy
            }
            _ => Tail::Plain,
        };
        groups[tail as usize].push(i.id);
    }

    let time = zone.now().wire() as u16;
    let energy = pos.energy as u16;
    let mut extra = pos.extra.unwrap_or_default();
    extra.energy = energy;
    let mut buf = Vec::with_capacity(32);
    for (idx, to) in groups.iter().enumerate() {
        if to.is_empty() {
            continue;
        }
        let tail = match idx {
            0 => PositionTail::None,
            1 => PositionTail::Energy(energy),
            _ => PositionTail::Extra(extra),
        };
        buf.clear();
        if send_wpn {
            S2CWeapon {
                rotation: pos.rotation,
                time,
                x: pos.x,
                y_speed: pos.y_speed,
                player_id: pid.0,
                x_speed: pos.x_speed,
                checksum: 0,
                status: pos.status,
                c2s_latency: latency,
                y: pos.y,
                bounty: pos.bounty,
                weapon: pos.weapon,
                tail,
            }
            .encode(&mut buf);
        } else {
            S2CPosition {
                rotation: pos.rotation,
                time,
                x: pos.x,
                c2s_latency: latency,
                bounty: pos.bounty as u8,
                player_id: pid.0 as u8,
                status: pos.status,
                y_speed: pos.y_speed,
                y: pos.y,
                x_speed: pos.x_speed,
                tail,
            }
            .encode(&mut buf);
        }
        zone.send_many(to.iter().copied(), &buf, flags);
    }
}

/// ASSS `SetFreqAndShip`: changes ship and frequency and tells the arena.
pub fn set_ship_and_freq(zone: &mut Zone, pid: PlayerId, ship: Ship, freq: u16) {
    let Some(p) = zone.players.get_mut(pid) else {
        return;
    };
    let Some(arena) = p.arena else { return };
    if p.kind == ClientKind::Chat && !ship.is_spec() {
        return;
    }
    if freq > 9999 || ship.0 > 8 || (p.ship == ship && p.freq == freq) {
        return;
    }
    p.ship = ship;
    p.freq = freq;
    if let Some(d) = gd(zone, pid) {
        d.speccing = None;
    }
    let mut buf = Vec::with_capacity(6);
    ShipChange {
        ship: ship.0 as i8,
        pid: pid.0 as i16,
        freq: freq as i16,
    }
    .encode(&mut buf);
    zone.send(pid, &buf, SendFlags::RELIABLE);
    zone.send_to_arena(arena, Some(pid), &buf, SendFlags::RELIABLE);
    zone.fire(&ShipFreqChange {
        player: pid,
        arena,
        ship,
        freq,
    });
    tracing::debug!(target: "game", pid = pid.0, ship = ship.0, freq, "changed ship/freq");
}

/// ASSS `SetFreq`: changes frequency only.
pub fn set_freq(zone: &mut Zone, pid: PlayerId, freq: u16) {
    let Some(p) = zone.players.get_mut(pid) else {
        return;
    };
    let Some(arena) = p.arena else { return };
    if freq > 9999 || p.freq == freq {
        return;
    }
    p.freq = freq;
    let ship = p.ship;
    let mut buf = Vec::with_capacity(6);
    FreqChange {
        pid: pid.0 as i16,
        freq: freq as i16,
        ship: 0xFF,
    }
    .encode(&mut buf);
    zone.send(pid, &buf, SendFlags::RELIABLE);
    zone.send_to_arena(arena, Some(pid), &buf, SendFlags::RELIABLE);
    zone.fire(&ShipFreqChange {
        player: pid,
        arena,
        ship,
        freq,
    });
}

fn on_set_ship(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 2 {
        tracing::warn!(target: "game", pid = pid.0, len = pkt.len(), "bad ship request length");
        return;
    }
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    let mut ship = Ship(pkt[1]);
    let mut freq = p.freq;
    if ship.0 > 8 {
        tracing::warn!(target: "game", pid = pid.0, ship = ship.0, "bad ship number");
        return;
    }
    if ship == p.ship {
        return;
    }
    if let Some(fm) = zone.iface_in::<dyn FreqManager>(arena) {
        fm.ship_change(zone, pid, &mut ship, &mut freq);
    }
    set_ship_and_freq(zone, pid, ship, freq);
}

fn on_set_freq(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 3 {
        tracing::warn!(target: "game", pid = pid.0, len = pkt.len(), "bad freq request length");
        return;
    }
    let Ok(req) = c2s::SetFreq::decode(pkt) else {
        return;
    };
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    let (mut ship, old_ship) = (p.ship, p.ship);
    let Ok(mut freq) = u16::try_from(req.freq) else {
        return;
    };
    if let Some(fm) = zone.iface_in::<dyn FreqManager>(arena) {
        fm.freq_change(zone, pid, &mut ship, &mut freq);
    }
    if ship != old_ship {
        set_ship_and_freq(zone, pid, ship, freq);
    } else {
        set_freq(zone, pid, freq);
    }
}

fn on_spec_request(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    let Ok(req) = c2s::SpecRequest::decode(pkt) else {
        return;
    };
    let target = u16::try_from(req.pid)
        .ok()
        .map(PlayerId)
        .filter(|t| zone.players.get(*t).is_some());
    if let Some(d) = gd(zone, pid) {
        d.speccing = target;
    }
}

/// Simple frequency manager (a subset of ASSS `fm_normal`): spectators go to
/// the spectator frequency, players to the least populated of the first
/// `Team:DesiredTeams` frequencies; requested frequencies above
/// `Team:MaxFrequency` are refused.
pub struct BalancedFreqs;

fn balanced(zone: &Zone, arena: ArenaId, exclude: PlayerId) -> u16 {
    let teams = zone
        .arenas
        .get(arena)
        .map_or(2, |a| a.cfg.get_int("Team", "DesiredTeams", 2))
        .max(1);
    let teams = u16::try_from(teams).unwrap_or(2);
    let mut counts = vec![0u32; usize::from(teams)];
    for p in zone.players.in_arena(arena) {
        if p.id != exclude && !p.ship.is_spec() && p.freq < teams {
            counts[usize::from(p.freq)] += 1;
        }
    }
    counts
        .iter()
        .enumerate()
        .min_by_key(|&(_, c)| *c)
        .map_or(0, |(i, _)| i as u16)
}

impl FreqManager for BalancedFreqs {
    fn initial_freq(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        let Some(arena) = zone.players.get(player).and_then(|p| p.arena) else {
            return;
        };
        *freq = if ship.is_spec() {
            zone.arenas.get(arena).map_or(8025, spec_freq)
        } else {
            balanced(zone, arena, player)
        };
    }

    fn ship_change(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        let Some(arena) = zone.players.get(player).and_then(|p| p.arena) else {
            return;
        };
        let spec = zone.arenas.get(arena).map_or(8025, spec_freq);
        if ship.is_spec() {
            *freq = spec;
        } else if *freq == spec {
            *freq = balanced(zone, arena, player);
        }
    }

    fn freq_change(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        let Some(p) = zone.players.get(player) else {
            return;
        };
        let Some(arena) = p.arena else { return };
        let current = p.freq;
        let spec = zone.arenas.get(arena).map_or(8025, spec_freq);
        let max = zone
            .arenas
            .get(arena)
            .map_or(0, |a| a.cfg.get_int("Team", "MaxFrequency", 0));
        if *freq == spec {
            *ship = Ship::SPEC;
        } else if max > 0 && i32::from(*freq) >= max {
            *freq = current;
        }
    }
}

/// The `game` module (also provides [`BalancedFreqs`] as the default
/// frequency manager).
pub struct GameModule;

impl Module for GameModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::POSITION, on_position);
        zone.on_packet(me, c2s::kind::SET_SHIP, on_set_ship);
        zone.on_packet(me, c2s::kind::SET_FREQ, on_set_freq);
        zone.on_packet(me, c2s::kind::SPEC_REQUEST, on_spec_request);
        if zone.iface::<dyn FreqManager>().is_none() {
            zone.reg
                .provide::<dyn FreqManager>(me, Scope::Global, Arc::new(BalancedFreqs));
        }
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action != Action::EnterArena {
                return;
            }
            let Some(arena) = ev.arena else { return };
            let Some(cfg) = z.arenas.get(arena).map(|a| a.cfg.clone()) else {
                return;
            };
            if let Some(d) = gd(z, ev.player) {
                *d = GameData {
                    see_nrg: SeeEnergy::from_cfg(cfg.get_int("Misc", "SeeEnergy", 0)),
                    see_nrg_spec: SeeEnergy::from_cfg(cfg.get_int("Misc", "SpecSeeEnergy", 0)),
                    see_epd: cfg.get_int("Misc", "SpecSeeExtra", 0) != 0,
                    ..GameData::default()
                };
            }
        });
        Ok(())
    }
}
