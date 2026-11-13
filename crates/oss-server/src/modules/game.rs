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
//! lagaction's weapon-ignore share drops that fraction of a player's weapons
//! before relaying them, and `General:ShipChangeLimit` throttles rapid ship
//! changes (ASSS: the count halves every 10 seconds; going over it blocks
//! changes for about 30 seconds).
//!
//! Every `Misc:RegionCheckInterval` ticks a player's position is checked
//! against the map's eLVL regions: entering or leaving one raises
//! [`RegionChange`] (ASSS `CB_REGION`), and inside no-weapons or
//! no-antiwarp regions weapons and antiwarp are stripped before relaying.
//!
//! Not yet ported: the "during change" ack wait.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::position::{C2SPosition, PositionTail, S2CPosition, S2CWeapon, status, weapon_type};
use oss_proto::s2c::{
    FreqChange, Green as S2CGreen, Kill as S2CKill, ShipChange, Turret, TurretKickoff,
};

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
    /// Ship before the change.
    pub old_ship: Ship,
    /// Frequency before the change.
    pub old_freq: u16,
    /// True for ASSS `CB_SHIPCHANGE` (a ship+freq change), false for
    /// `CB_FREQCHANGE` (frequency only).
    pub ship_change: bool,
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
    /// fired a weapon since the last death
    sent_wpn: bool,
    /// deaths in a row without firing
    deaths_without_firing: i32,
    /// ship locked by staff (`?lock`)
    locked: bool,
    /// share of this player's weapons to drop (lagaction)
    ignore_weapons: f64,
    /// weapon packets relayed to this player since entering the arena
    wpn_sent: u64,
    /// recent ship changes (halved every 10 seconds) and when last decayed
    changes: u32,
    changes_checked: crate::time::Ticks,
    /// regions the player was in at the last check (ascending ids)
    regions: Vec<usize>,
    region_checked: crate::time::Ticks,
    rgn_no_anti: bool,
    rgn_no_weapons: bool,
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

/// Buffers [`relay`] reuses between packets: one recipient list per position
/// tail, and the packet it builds for them.
///
/// These used to be built from nothing for every position packet, which at
/// 500 players was about two thirds of the zone's allocation traffic -- each
/// list grew 8, 16, 32 ... bytes by doubling, every packet, and was then
/// dropped. They are taken out of the registry for the call and put back
/// emptied, so each keeps the capacity the arena's fan-out has needed.
#[derive(Default)]
struct RelayScratch {
    groups: [Vec<PlayerId>; 3],
    buf: Vec<u8>,
}

/// Which variant of the outgoing packet a recipient gets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    Plain,
    Energy,
    Extra,
}

/// A player entered or left an eLVL region (ASSS `CB_REGION`).
#[derive(Debug, Clone)]
pub struct RegionChange {
    /// The player.
    pub player: PlayerId,
    /// Their arena.
    pub arena: ArenaId,
    /// The region's id (index in [`super::mapdata::ArenaMap::regions`]).
    pub region: usize,
    /// Tile position at the check.
    pub x: i32,
    /// Tile position at the check.
    pub y: i32,
    /// True when entering, false when leaving.
    pub entering: bool,
}

impl Event for RegionChange {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// `Misc:RegionCheckInterval`, cached per arena.
struct RegionInterval(u64);

/// The regions a player was in at the last check.
pub fn player_regions(zone: &Zone, pid: PlayerId) -> Vec<usize> {
    zone.players
        .get(pid)
        .and_then(|p| p.ext.get::<GameData>())
        .map(|d| d.regions.clone())
        .unwrap_or_default()
}

/// ASSS `update_regions`: diffs the regions at tile (`x`, `y`) with the last
/// set and raises enter/exit events.
fn update_regions(zone: &mut Zone, pid: PlayerId, arena: ArenaId, x: i32, y: i32) {
    let (now_in, no_anti, no_wpn) = match super::mapdata::arena_map(zone, arena) {
        Some(m) if !m.regions.is_empty() => {
            let ids: Vec<usize> = m.regions_at(x, y).collect();
            let anti = ids.iter().any(|&i| m.regions[i].no_antiwarp);
            let wpn = ids.iter().any(|&i| m.regions[i].no_weapons);
            (ids, anti, wpn)
        }
        _ => (Vec::new(), false, false),
    };
    let Some(d) = gd(zone, pid) else { return };
    d.rgn_no_anti = no_anti;
    d.rgn_no_weapons = no_wpn;
    if d.regions == now_in {
        return;
    }
    let old = std::mem::replace(&mut d.regions, now_in.clone());
    let ev = |region, entering| RegionChange {
        player: pid,
        arena,
        region,
        x,
        y,
        entering,
    };
    for r in old.iter().filter(|r| !now_in.contains(r)) {
        zone.fire(&ev(*r, false));
    }
    for r in now_in.iter().filter(|r| !old.contains(r)) {
        zone.fire(&ev(*r, true));
    }
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
    if !fake {
        let wpn_sent = zone
            .players
            .get(pid)
            .and_then(|p| p.ext.get::<GameData>())
            .map_or(0, |d| d.wpn_sent);
        super::lagdata::position(zone, pid, i32::from(latency) * 10, wpn_sent);
    }
    let (old_time, old_status, had) = zone
        .players
        .get(pid)
        .and_then(|p| p.ext.get::<GameData>())
        .map_or((0, 0, false), |d| (d.time, d.status, d.have_pos));
    let is_newer = !had || (pos.time.wrapping_sub(old_time) as i32) > 0;

    if !ship.is_spec() {
        // region tracking, for real players, once in a while
        let interval = zone
            .arenas
            .get(arena)
            .and_then(|a| a.ext.get::<RegionInterval>())
            .map_or(100, |r| r.0);
        let due = zone
            .players
            .get(pid)
            .and_then(|p| p.ext.get::<GameData>())
            .is_some_and(|d| now.since(d.region_checked) >= interval);
        if is_newer && !fake && due {
            update_regions(
                zone,
                pid,
                arena,
                i32::from(pos.x) >> 4,
                i32::from(pos.y) >> 4,
            );
            if let Some(d) = gd(zone, pid) {
                d.region_checked = now;
            }
        }
        if pos.weapon.kind() != weapon_type::NONE
            && let Some(d) = gd(zone, pid)
        {
            d.sent_wpn = true;
            d.deaths_without_firing = 0;
        }
        // lagaction's weapon-ignore share (after the "fired" bookkeeping)
        let (ignore, no_wpn, no_anti) = zone
            .players
            .get(pid)
            .and_then(|p| p.ext.get::<GameData>())
            .map_or((0.0, false, false), |d| {
                (d.ignore_weapons, d.rgn_no_weapons, d.rgn_no_anti)
            });
        if (ignore > 0.0 && f64::from(rand(zone).below(1 << 16)) < ignore * 65536.0) || no_wpn {
            pos.weapon.set_kind(weapon_type::NONE);
        }
        // and antiwarp, in no-antiwarp regions
        if no_anti {
            pos.status &= !status::ANTIWARP;
        }
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

    let see_own = zone
        .players
        .get(pid)
        .and_then(|p| p.ext.get::<BotFeatures>())
        .is_some_and(|b| b.see_own_posn);
    let mut scratch = std::mem::take(zone.res.get_or_default::<RelayScratch>());
    let groups = &mut scratch.groups;
    for i in zone.players.iter() {
        if i.status != PlayerStatus::Playing
            || i.arena != Some(arena)
            || (i.id == pid && !see_own)
            || !matches!(i.kind, ClientKind::Vie | ClientKind::Continuum)
        {
            continue;
        }
        // a bot that asked for every position packet (`?botfeature`)
        let see_all = i.ext.get::<BotFeatures>().is_some_and(|b| b.see_all_posn);
        let d = i.ext.get::<GameData>();
        let (ix, iy) = d.map_or((0, 0), |d| (i64::from(d.x), i64::from(d.y)));
        let dist = (((x1 - ix).pow(2) + (y1 - iy).pow(2)) as f64).sqrt() as i64;
        let watching = d.is_some_and(|d| d.speccing == Some(pid));
        let turret = i.attached == Some(pid);
        let radar = dist <= cfg.pos_pix && dist < radar_cut;
        let near = if send_wpn {
            dist <= range || (wtype == weapon_type::NONE && radar)
        } else {
            let res = i64::from(i.resolution.0) + i64::from(i.resolution.1);
            dist < res || radar
        };
        if !(near || to_all || watching || turret || see_all) {
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
    for idx in 0..scratch.groups.len() {
        if scratch.groups[idx].is_empty() {
            continue;
        }
        let tail = match idx {
            0 => PositionTail::None,
            1 => PositionTail::Energy(energy),
            _ => PositionTail::Extra(extra),
        };
        scratch.buf.clear();
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
            .encode(&mut scratch.buf);
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
            .encode(&mut scratch.buf);
        }
        zone.send_many(scratch.groups[idx].iter().copied(), &scratch.buf, flags);
    }
    // weapon packets each recipient was sent, for weapon loss
    if wtype != weapon_type::NONE {
        for i in 0..scratch.groups.len() {
            for j in 0..scratch.groups[i].len() {
                let id = scratch.groups[i][j];
                if let Some(d) = gd(zone, id) {
                    d.wpn_sent += 1;
                }
            }
        }
    }
    for g in &mut scratch.groups {
        g.clear();
    }
    scratch.buf.clear();
    *zone.res.get_or_default::<RelayScratch>() = scratch;
}

/// ASSS `SetIgnoreWeapons`: drop this share (0 to 1) of a player's weapons.
pub fn set_ignore_weapons(zone: &mut Zone, pid: PlayerId, proportion: f64) {
    if let Some(d) = gd(zone, pid) {
        d.ignore_weapons = proportion.clamp(0.0, 1.0);
    }
}

/// The share of a player's weapons being dropped.
pub fn ignore_weapons(zone: &Zone, pid: PlayerId) -> f64 {
    zone.players
        .get(pid)
        .and_then(|p| p.ext.get::<GameData>())
        .map_or(0.0, |d| d.ignore_weapons)
}

/// ASSS `General:ShipChangeLimit`: counts a ship change request; false if
/// the player has made too many lately.
fn ship_change_allowed(zone: &mut Zone, pid: PlayerId) -> bool {
    let limit =
        u32::try_from(zone.cfg.global().get_int("General", "ShipChangeLimit", 10)).unwrap_or(0);
    let now = zone.now();
    let Some(d) = gd(zone, pid) else {
        return true;
    };
    // exponential decay by 1/2 every 10 seconds
    let periods = now.since(d.changes_checked) / 1000;
    d.changes = d
        .changes
        .checked_shr(u32::try_from(periods).unwrap_or(u32::MAX))
        .unwrap_or(0);
    d.changes_checked = crate::time::Ticks(d.changes_checked.0 + periods * 1000);
    if limit > 0 && d.changes > limit {
        // blocked for at least 30 seconds
        d.changes |= limit << 3;
        tracing::info!(target: "game", pid = pid.0, "too many ship changes");
        super::chat::message(
            zone,
            pid,
            "You're changing ships too often, disabling for 30 seconds.",
        );
        return false;
    }
    d.changes += 1;
    true
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
    let (old_ship, old_freq) = (p.ship, p.freq);
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
        old_ship,
        old_freq,
        ship_change: true,
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
    let old_freq = p.freq;
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
        old_ship: ship,
        old_freq,
        ship_change: false,
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
    if !ship_change_allowed(zone, pid) {
        return;
    }
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let locked = p.ext.get::<GameData>().is_some_and(|d| d.locked);
    if locked
        && !ship.is_spec()
        && !zone
            .iface::<dyn super::capman::Capabilities>()
            .is_some_and(|c| c.has_capability(zone, pid, "bypasslock"))
    {
        let spec = zone.players.get(pid).is_some_and(|p| p.ship.is_spec());
        super::chat::message(
            zone,
            pid,
            if spec {
                "You have been locked in spectator mode."
            } else {
                "You have been locked in your ship."
            },
        );
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

/// ASSS `fm_normal`: the frequency manager that decides what ship and
/// frequency a player gets when they enter an arena, change ship or ask for
/// another frequency.
///
/// Every setting `fm_normal.c` reads is read here: `Team:DesiredTeams`,
/// `MaxFrequency`, `PrivFreqStart`, `MaxPerTeam`, `MaxPerPrivateTeam`,
/// `IncludeSpectators`, `ForceEvenTeams`, `FrequencyShipTypes`,
/// `InitialSpec`, `General:MaxPlaying` and `Misc:MaxXres`/`MaxYres`. On top
/// of ASSS's `no_ship` lag flag it also keeps a player in spectator mode
/// while the lag or anti-cheat checks say so.
pub struct BalancedFreqs;

/// What `fm_normal` reads from an arena's config.
#[derive(Debug, Clone, Copy)]
struct FmCfg {
    desired: u16,
    max_freq: i32,
    priv_start: i32,
    max_per_team: i32,
    max_per_private: i32,
    incl_spec: bool,
    force_even: bool,
    ship_types: bool,
    max_playing: i32,
    initial_spec: bool,
    max_x: i32,
    max_y: i32,
    spec_freq: u16,
}

/// ASSS `CFG_MAX_DESIRED`.
const MAX_DESIRED: i32 = 10;

fn fm_cfg(zone: &Zone, arena: ArenaId) -> FmCfg {
    let Some(a) = zone.arenas.get(arena) else {
        return FmCfg {
            desired: 2,
            max_freq: 10000,
            priv_start: 100,
            max_per_team: 0,
            max_per_private: 0,
            incl_spec: false,
            force_even: false,
            ship_types: false,
            max_playing: 100,
            initial_spec: false,
            max_x: 0,
            max_y: 0,
            spec_freq: 8025,
        };
    };
    let c = &a.cfg;
    FmCfg {
        desired: c.get_int("Team", "DesiredTeams", 2).clamp(1, MAX_DESIRED) as u16,
        max_freq: c.get_int("Team", "MaxFrequency", 10000),
        priv_start: c.get_int("Team", "PrivFreqStart", 100),
        max_per_team: c.get_int("Team", "MaxPerTeam", 0),
        max_per_private: c.get_int("Team", "MaxPerPrivateTeam", 0),
        incl_spec: c.get_int("Team", "IncludeSpectators", 0) != 0,
        force_even: c.get_int("Team", "ForceEvenTeams", 0) != 0,
        ship_types: c.get_int("Team", "FrequencyShipTypes", 0) != 0,
        max_playing: c.get_int("General", "MaxPlaying", 100),
        initial_spec: c.get_int("Team", "InitialSpec", 0) != 0,
        max_x: c.get_int("Misc", "MaxXres", 0),
        max_y: c.get_int("Misc", "MaxYres", 0),
        spec_freq: spec_freq(a),
    }
}

impl FmCfg {
    /// ASSS `get_max_for_freq`: private frequencies have their own limit.
    fn max_for_freq(&self, freq: u16) -> i32 {
        if freq > 0 && i32::from(freq) >= self.priv_start {
            self.max_per_private
        } else {
            self.max_per_team
        }
    }
}

/// ASSS `count_current_playing`.
fn count_playing(zone: &Zone, arena: ArenaId) -> i32 {
    zone.players
        .in_arena(arena)
        .filter(|p| p.status == PlayerStatus::Playing && !p.ship.is_spec())
        .count() as i32
}

/// ASSS `count_freq`.
fn count_freq(zone: &Zone, arena: ArenaId, freq: u16, excl: PlayerId, incl_spec: bool) -> i32 {
    zone.players
        .in_arena(arena)
        .filter(|p| p.freq == freq && p.id != excl && (!p.ship.is_spec() || incl_spec))
        .count() as i32
}

/// ASSS `BalanceFreqs`: the least populated of the first `DesiredTeams`
/// frequencies, picking at random between ties; if they are all full, the
/// first frequency above them with room.
fn balanced(zone: &mut Zone, arena: ArenaId, excl: PlayerId, cfg: &FmCfg) -> u16 {
    let max = cfg.max_for_freq(0);
    let mut counts = vec![0i32; usize::from(cfg.desired)];
    for p in zone.players.in_arena(arena) {
        if p.freq < cfg.desired && p.id != excl && (!p.ship.is_spec() || cfg.incl_spec) {
            counts[usize::from(p.freq)] += 1;
        }
    }
    let min = counts.iter().copied().min().unwrap_or(0);
    let best: Vec<u16> = counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == min)
        .map(|(i, _)| i as u16)
        .collect();
    if best.is_empty() {
        return 0;
    }
    if max == 0 || min < max {
        let i = rand(zone).below(best.len() as u32) as usize;
        return best[i];
    }
    // every desired freq is full: walk up until one has room
    let mut f = cfg.desired;
    while count_freq(zone, arena, f, excl, cfg.incl_spec) >= max {
        let Some(next) = f.checked_add(1) else { break };
        f = next;
    }
    f
}

/// ASSS `FindLegalShip`: with `Team:FrequencyShipTypes`, freq 0 flies
/// warbirds, freq 1 javelins and so on, and any other freq only spectates.
fn legal_ship(cfg: &FmCfg, freq: u16, ship: Ship) -> Ship {
    if !cfg.ship_types {
        return ship;
    }
    // speccers are left alone, even here
    if ship.is_spec() || freq > 7 {
        Ship::SPEC
    } else {
        Ship(freq as u8)
    }
}

/// ASSS `screen_res_allowed`, which tells the player itself.
fn res_allowed(zone: &mut Zone, player: PlayerId, cfg: &FmCfg) -> bool {
    let Some(p) = zone.players.get(player) else {
        return true;
    };
    let (x, y) = (i32::from(p.resolution.0), i32::from(p.resolution.1));
    if (cfg.max_x == 0 || x <= cfg.max_x) && (cfg.max_y == 0 || y <= cfg.max_y) {
        return true;
    }
    let (mx, my) = (cfg.max_x, cfg.max_y);
    super::chat::message(
        zone,
        player,
        &format!("Maximum allowed screen resolution is {mx}x{my} in this arena"),
    );
    false
}

/// The lag and anti-cheat holds, which are this server's `p->flags.no_ship`.
/// `notify` sends the reason, as ASSS's ship change does.
fn held_in_spec(zone: &mut Zone, player: PlayerId, notify: bool) -> bool {
    if super::lagdata::no_ship(zone, player) {
        if notify {
            super::chat::message(zone, player, "You have too much lag to play in this arena.");
        }
        return true;
    }
    if super::anticheat::no_ship(zone, player) {
        if notify {
            super::chat::message(
                zone,
                player,
                "The anti-cheat checks are keeping you in spectator mode for now.",
            );
        }
        return true;
    }
    false
}

/// Where the balancer puts a player, falling back to spectator mode when
/// even that frequency is out of range (ASSS does this at each call site).
fn place(zone: &mut Zone, arena: ArenaId, player: PlayerId, cfg: &FmCfg, ship: &mut Ship) -> u16 {
    let f = balanced(zone, arena, player, cfg);
    if i32::from(f) >= cfg.max_freq {
        *ship = Ship::SPEC;
        cfg.spec_freq
    } else {
        f
    }
}

impl FreqManager for BalancedFreqs {
    fn initial_freq(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        let Some(arena) = zone.players.get(player).and_then(|p| p.arena) else {
            return;
        };
        let cfg = fm_cfg(zone, arena);
        let too_many = cfg.max_playing > 0 && count_playing(zone, arena) >= cfg.max_playing;
        if too_many
            || held_in_spec(zone, player, false)
            || !res_allowed(zone, player, &cfg)
            || cfg.initial_spec
            // the arena is locked to spectator mode (`?lockarena`)
            || arena_lock(zone, arena).spec
        {
            *ship = Ship::SPEC;
        }
        if ship.is_spec() {
            *freq = cfg.spec_freq;
        } else {
            *freq = place(zone, arena, player, &cfg, ship);
            *ship = legal_ship(&cfg, *freq, *ship);
        }
    }

    fn ship_change(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        let Some(arena) = zone.players.get(player).and_then(|p| p.arena) else {
            return;
        };
        let cfg = fm_cfg(zone, arena);
        let Some((was_spec, old_ship, old_freq)) = zone
            .players
            .get(player)
            .map(|p| (p.ship.is_spec(), p.ship, p.freq))
        else {
            return;
        };
        let mut deny = false;
        if ship.is_spec() {
            // changing to spectator mode is always allowed
            *freq = cfg.spec_freq;
        } else if held_in_spec(zone, player, true) || !res_allowed(zone, player, &cfg) {
            deny = true;
        } else if was_spec && cfg.max_playing > 0 && count_playing(zone, arena) >= cfg.max_playing {
            super::chat::message(
                zone,
                player,
                "There are too many people playing in this arena.",
            );
            deny = true;
        } else {
            let mut need_balance = false;
            if was_spec && *freq == cfg.spec_freq {
                // leaving spec on the spec freq: always reassign
                need_balance = true;
            } else if cfg.ship_types {
                *freq = u16::from(ship.0);
                self.freq_change(zone, player, ship, freq);
                return;
            } else if was_spec {
                // unspeccing onto a real freq: only reassign if it is full
                let max = cfg.max_for_freq(*freq);
                if max > 0 && count_freq(zone, arena, *freq, player, false) >= max {
                    need_balance = true;
                }
            }
            if need_balance {
                *freq = place(zone, arena, player, &cfg, ship);
            }
            *ship = legal_ship(&cfg, *freq, *ship);
        }
        if deny {
            (*ship, *freq) = (old_ship, old_freq);
        }
    }

    fn freq_change(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16) {
        let Some(p) = zone.players.get(player) else {
            return;
        };
        let Some(arena) = p.arena else { return };
        let (old_ship, old_freq, was_spec) = (p.ship, p.freq, p.ship.is_spec());
        let cfg = fm_cfg(zone, arena);
        // Asking for the spectator frequency is how a client asks to
        // spectate (`=8025`), so it always lands in spectator mode. ASSS
        // only short-circuits a spectator who is already there, and with
        // its default `MaxFrequency` of 10000 a player in a ship who asks
        // for 8025 keeps the ship and takes the frequency with them; no
        // zone wants that, so this is a deliberate divergence.
        if *freq == cfg.spec_freq {
            *ship = Ship::SPEC;
            return;
        }
        if i32::from(*freq) >= cfg.max_freq {
            *freq = place(zone, arena, player, &cfg, ship);
        } else {
            let count = count_freq(zone, arena, *freq, player, cfg.incl_spec);
            let max = cfg.max_for_freq(*freq);
            if max > 0 && count >= max {
                *freq = place(zone, arena, player, &cfg, ship);
            } else if cfg.force_even {
                let old = count_freq(zone, arena, old_freq, player, cfg.incl_spec);
                if old < count {
                    super::chat::message(
                        zone,
                        player,
                        "Changing frequencies would make the teams uneven.",
                    );
                    (*ship, *freq) = (old_ship, old_freq);
                    return;
                }
            }
        }
        *ship = legal_ship(&cfg, *freq, *ship);
        // did that bring them out of spec when the arena is already full?
        if !ship.is_spec()
            && was_spec
            && cfg.max_playing > 0
            && count_playing(zone, arena) >= cfg.max_playing
        {
            (*ship, *freq) = (old_ship, old_freq);
            super::chat::message(
                zone,
                player,
                "There are too many people playing in this arena.",
            );
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
        zone.on_packet(me, c2s::kind::DEATH, on_death);
        zone.on_packet(me, c2s::kind::TAKE_GREEN, on_green);
        zone.on_packet(me, c2s::kind::ATTACH_TO, on_attach);
        zone.on_packet(me, c2s::kind::TURRET_KICKOFF, on_kickoff);
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action == Action::PreEnterArena
                && let Some(p) = z.players.get_mut(ev.player)
            {
                p.attached = None;
            }
            if ev.action == Action::LeaveArena
                && let Some(d) = gd(z, ev.player)
            {
                d.regions.clear();
            }
        });
        zone.on::<super::arenaman::ArenaAction>(me, Scope::Global, |z, ev| {
            use super::arenaman::ArenaActionKind;
            if matches!(
                ev.action,
                ArenaActionKind::Create | ArenaActionKind::ConfChanged
            ) && let Some(a) = z.arenas.get_mut(ev.arena)
            {
                let every = a.cfg.get_int("Misc", "RegionCheckInterval", 100);
                a.ext
                    .insert(RegionInterval(u64::try_from(every).unwrap_or(100)));
            }
        });
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
            let now = z.now();
            let locked = arena_lock(z, arena).lock_ship;
            if let Some(d) = gd(z, ev.player) {
                let ignore_weapons = d.ignore_weapons;
                *d = GameData {
                    ignore_weapons,
                    locked,
                    changes_checked: now,
                    region_checked: now,
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

/// ASSS `CB_KILL`: a player died. Handlers add kill points to `points` and
/// may pick the prize left behind by setting `green` (-1 = let the server pick
/// a weighted random prize). Raised before the kill is announced.
#[derive(Debug)]
pub struct Kill {
    /// The arena.
    pub arena: ArenaId,
    /// Who killed.
    pub killer: PlayerId,
    /// Who died.
    pub killed: PlayerId,
    /// The killed player's bounty, as reported by their client.
    pub bounty: i16,
    /// Flags the killed player carried.
    pub flags: i16,
    /// Kill points to award (sent in the kill packet).
    pub points: std::cell::Cell<i32>,
    /// Prize left at the death spot.
    pub green: std::cell::Cell<i32>,
}

impl Event for Kill {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_KILL_POST_NOTIFY`: raised after the kill packet was sent, with
/// the final points and prize.
#[derive(Debug, Clone, Copy)]
pub struct KillAnnounced {
    /// The arena.
    pub arena: ArenaId,
    /// Who killed.
    pub killer: PlayerId,
    /// Who died.
    pub killed: PlayerId,
    /// Points awarded.
    pub points: i32,
    /// Flags transferred.
    pub flags: i16,
    /// Prize left behind.
    pub green: i32,
}

impl Event for KillAnnounced {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_GREEN`: a player picked up a prize.
#[derive(Debug, Clone, Copy)]
pub struct GreenPickup {
    /// The arena.
    pub arena: ArenaId,
    /// Who picked it up.
    pub player: PlayerId,
    /// Tile x.
    pub x: i16,
    /// Tile y.
    pub y: i16,
    /// Prize number (negative = bad prize).
    pub prize: i16,
    /// Whether the green was relayed to the rest of the arena.
    ///
    /// Not part of ASSS's `CB_GREEN`, which has no equivalent: it matters
    /// because `Prize:DontShareThor`, `DontShareBurst` and `DontShareBrick`
    /// hold the broadcast back, and a green nobody hears about is a green
    /// no teammate's client shares (`Ship:PrizeShareLimit`). A listener that
    /// tracks what a player may be holding needs to know which it was.
    pub broadcast: bool,
}

impl Event for GreenPickup {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// True once the player has sent a position packet in their current arena
/// (which means the client has the map and settings).
pub fn has_sent_position(zone: &Zone, player: PlayerId) -> bool {
    zone.players
        .get(player)
        .and_then(|p| p.ext.get::<GameData>())
        .is_some_and(|d| d.have_pos)
}

fn on_death(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 5 {
        tracing::warn!(target: "game", pid = pid.0, len = pkt.len(), "bad death packet length");
        return;
    }
    let Ok(d) = c2s::Death::decode(pkt) else {
        return;
    };
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        tracing::warn!(target: "game", pid = pid.0, "death packet from bad state");
        return;
    };
    let killer = u16::try_from(d.killer).ok().map(PlayerId);
    let Some(killer) =
        killer.filter(|k| zone.players.get(*k).is_some_and(|k| k.is_playing_in(arena)))
    else {
        tracing::warn!(target: "game", pid = pid.0, killer = d.killer, "reported kill by bad pid");
        return;
    };
    let flags = zone.players.get(pid).map_or(0, |p| p.flags_carried);
    let ev = Kill {
        arena,
        killer,
        killed: pid,
        bounty: d.bounty,
        flags,
        points: std::cell::Cell::new(0),
        green: std::cell::Cell::new(-1),
    };
    zone.fire(&ev);
    let pts = ev.points.get();
    if pts != 0
        && let Some(st) = zone.iface_in::<dyn super::stats::Stats>(arena)
    {
        st.increment(zone, killer, super::stats::stat::KILL_POINTS, pts);
    }
    let mut green = ev.green.get();
    if green == -1 {
        green = match zone.iface_in::<dyn super::clientset::ClientSettings>(arena) {
            Some(cs) => {
                let mut rng = std::mem::take(rand(zone));
                let g = cs.random_prize(zone, arena, &mut |n| rng.below(n));
                *rand(zone) = rng;
                i32::from(g)
            }
            None => 0,
        };
    }
    let mut buf = Vec::with_capacity(11);
    S2CKill {
        green: green as u8,
        killer: killer.0 as i16,
        killed: pid.0 as i16,
        bounty: pts as i16,
        flags,
    }
    .encode(&mut buf);
    zone.send_to_arena(arena, None, &buf, SendFlags::RELIABLE);
    zone.fire(&KillAnnounced {
        arena,
        killer,
        killed: pid,
        points: pts,
        flags,
        green,
    });
    tracing::debug!(target: "game", killed = pid.0, killer = killer.0, bounty = d.bounty, pts, "kill");

    // spectate players who keep dying without ever firing
    let max = zone
        .arenas
        .get(arena)
        .map_or(5, |a| a.cfg.get_int("Security", "MaxDeathWithoutFiring", 5));
    let spec = zone.arenas.get(arena).map_or(8025, spec_freq);
    let Some(d) = gd(zone, pid) else { return };
    let too_many = if d.sent_wpn {
        false
    } else {
        let before = d.deaths_without_firing;
        d.deaths_without_firing += 1;
        before == max
    };
    d.sent_wpn = false;
    if too_many {
        tracing::debug!(target: "game", pid = pid.0, "specced for too many deaths without firing");
        set_ship_and_freq(zone, pid, Ship::SPEC, spec);
    }
}

fn on_green(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 11 {
        tracing::warn!(target: "game", pid = pid.0, len = pkt.len(), "bad green packet length");
        return;
    }
    let Ok(g) = c2s::TakeGreen::decode(pkt) else {
        return;
    };
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    let cfg = zone.arenas.get(arena).map(|a| a.cfg.clone());
    let personal = |key: &str| {
        cfg.as_ref()
            .is_some_and(|c| c.get_int("Prize", key, 0) != 0)
    };
    let private = (g.prize == oss_proto::common::prize::THOR && personal("DontShareThor"))
        || (g.prize == oss_proto::common::prize::BURST && personal("DontShareBurst"))
        || (g.prize == oss_proto::common::prize::BRICK && personal("DontShareBrick"));
    if !private {
        let mut buf = Vec::with_capacity(11);
        S2CGreen {
            time: g.time,
            x: g.x,
            y: g.y,
            prize: g.prize,
            pid: pid.0 as i16,
        }
        .encode(&mut buf);
        zone.send_to_arena(arena, Some(pid), &buf, SendFlags::UNRELIABLE);
    }
    zone.fire(&GreenPickup {
        arena,
        player: pid,
        x: g.x,
        y: g.y,
        prize: g.prize,
        broadcast: !private,
    });
}

/// ASSS `CB_ATTACH`: a player attached to (or detached from) a host.
#[derive(Debug, Clone, Copy)]
pub struct Attach {
    /// Arena.
    pub arena: ArenaId,
    /// The turret.
    pub player: PlayerId,
    /// The host, or `None` when detaching.
    pub host: Option<PlayerId>,
}

impl Event for Attach {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

fn on_attach(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 3 {
        tracing::warn!(target: "game", pid = pid.0, len = pkt.len(), "bad attach packet length");
        return;
    }
    let Ok(req) = c2s::AttachTo::decode(pkt) else {
        return;
    };
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    let freq = p.freq;
    let host = if req.pid == -1 {
        None
    } else {
        let to = u16::try_from(req.pid).ok().map(PlayerId);
        let ok = to.and_then(|t| zone.players.get(t)).is_some_and(|t| {
            t.status == PlayerStatus::Playing
                && t.id != pid
                && t.arena == Some(arena)
                && t.freq == freq
        });
        if !ok {
            tracing::warn!(target: "game", pid = pid.0, host = req.pid, "tried to attach to a bad pid");
            return;
        }
        to
    };
    if zone.players.get(pid).is_some_and(|p| p.attached == host) {
        return;
    }
    if let Some(p) = zone.players.get_mut(pid) {
        p.attached = host;
    }
    let mut buf = Vec::with_capacity(5);
    Turret {
        pid: pid.0 as i16,
        host: host.map_or(-1, |h| h.0 as i16),
    }
    .encode(&mut buf);
    zone.send_to_arena(arena, None, &buf, SendFlags::RELIABLE);
    zone.fire(&Attach {
        arena,
        player: pid,
        host,
    });
}

fn on_kickoff(zone: &mut Zone, pid: PlayerId, _pkt: &[u8]) {
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    let mut buf = Vec::with_capacity(3);
    TurretKickoff { pid: pid.0 as i16 }.encode(&mut buf);
    zone.send_to_arena(arena, None, &buf, SendFlags::RELIABLE);
}

/// Locks or unlocks a player's ship (ASSS `Lock`/`Unlock`): a locked
/// player can only change to spectator. `spec` first moves them to spec.
pub fn set_lock(zone: &mut Zone, pid: PlayerId, locked: bool, spec: bool) {
    set_lock_notify(zone, pid, locked, spec, false);
}

/// [`set_lock`], and with `notify` the player is told when it changes
/// anything (ASSS `lock_work`).
pub fn set_lock_notify(zone: &mut Zone, pid: PlayerId, locked: bool, spec: bool, notify: bool) {
    if locked && spec {
        let arena = zone.players.get(pid).and_then(|p| p.arena);
        if let Some(a) = arena {
            let sf = zone.arenas.get(a).map_or(8025, spec_freq);
            set_ship_and_freq(zone, pid, Ship::SPEC, sf);
        }
    }
    let was = zone
        .players
        .get(pid)
        .and_then(|p| p.ext.get::<GameData>())
        .is_some_and(|d| d.locked);
    if let Some(d) = gd(zone, pid) {
        d.locked = locked;
    }
    if notify && was != locked {
        let in_spec = zone.players.get(pid).is_some_and(|p| p.ship.is_spec());
        let text = match (locked, in_spec) {
            (true, true) => "You have been locked to spectator mode.",
            (true, false) => "You have been locked to your ship.",
            (false, _) => "Your ship has been unlocked.",
        };
        super::chat::message(zone, pid, text);
    }
}

/// What a bot asked the server to send it (ASSS's `p->flags.see_all_posn`
/// and `see_own_posn`, set with `?botfeature`). Unlike the rest of a
/// player's game state these outlive an arena change, as they do in ASSS.
#[derive(Debug, Clone, Copy, Default)]
pub struct BotFeatures {
    /// Every position packet in the arena, however far away.
    pub see_all_posn: bool,
    /// The player's own position packets, echoed back.
    pub see_own_posn: bool,
}

/// Turns one of ASSS's two bot features on or off. False if the name isn't
/// one of `seeallposn` or `seeownposn`.
pub fn set_bot_feature(zone: &mut Zone, pid: PlayerId, feature: &str, on: bool) -> bool {
    let Some(p) = zone.players.get_mut(pid) else {
        return false;
    };
    let b = p.ext.get_or_default::<BotFeatures>();
    match feature {
        "seeallposn" => b.see_all_posn = on,
        "seeownposn" => b.see_own_posn = on,
        _ => return false,
    }
    true
}

/// An arena's default locked state (ASSS's `initlockship` / `initspec`),
/// which `?lockarena` sets and entering players inherit.
#[derive(Debug, Clone, Copy, Default)]
struct ArenaLock {
    /// entering players are locked to the ship they come in with
    lock_ship: bool,
    /// entering players are put in spectator mode first
    spec: bool,
}

fn arena_lock(zone: &Zone, arena: ArenaId) -> ArenaLock {
    zone.arenas
        .get(arena)
        .and_then(|a| a.ext.get::<ArenaLock>())
        .copied()
        .unwrap_or_default()
}

/// ASSS `LockArena`: entering players are locked from now on, and unless
/// `only_arena` everyone already here is locked too. `initial` locks them to
/// the ship they arrive in rather than to spectator mode; `spec` moves the
/// players already here to spectator mode first; `notify` tells them.
pub fn lock_arena(
    zone: &mut Zone,
    arena: ArenaId,
    notify: bool,
    only_arena: bool,
    initial: bool,
    spec: bool,
) {
    if let Some(a) = zone.arenas.get_mut(arena) {
        let l = a.ext.get_or_default::<ArenaLock>();
        l.lock_ship = true;
        l.spec |= !initial;
    }
    if !only_arena {
        for p in zone
            .players
            .in_arena(arena)
            .map(|p| p.id)
            .collect::<Vec<_>>()
        {
            set_lock_notify(zone, p, true, spec, notify);
        }
    }
}

/// ASSS `UnlockArena`.
pub fn unlock_arena(zone: &mut Zone, arena: ArenaId, notify: bool, only_arena: bool) {
    if let Some(a) = zone.arenas.get_mut(arena) {
        *a.ext.get_or_default::<ArenaLock>() = ArenaLock::default();
    }
    if !only_arena {
        for p in zone
            .players
            .in_arena(arena)
            .map(|p| p.id)
            .collect::<Vec<_>>()
        {
            set_lock_notify(zone, p, false, false, notify);
        }
    }
}
