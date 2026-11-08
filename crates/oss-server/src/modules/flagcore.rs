//! `flagcore`: flag state, flag packets and the flag-game interface (ASSS
//! `flagcore.c`).
//!
//! Flagcore keeps every flag's state (none, carried, on the map) and turns
//! state changes into the right client packets. The rules of a particular
//! game (where flags spawn, what happens when one is dropped, when a team
//! wins) come from a [`FlagGame`] attached to the arena, e.g. the warzone
//! game in [`super::fg_wz`] or the turf game in [`super::fg_turf`].
//!
//! Carry modes follow ASSS: [`CARRY_NONE`] is a turf game (flags never leave
//! the map, only their owner changes), [`CARRY_ALL`] lets a player carry any
//! number, and `n >= 2` lets a player carry `n - 1`. The mode is also forced
//! into the arena's `Flag:CarryFlags` client setting.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::s2c::{FlagDrop, FlagLocation, FlagPickup, FlagVictory, TurfFlags};

use crate::arena::ArenaId;
use crate::clientset::OverrideKey;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus};
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::clientset::ClientSettings;
use super::core::{Action, NewPlayer, PlayerAction};
use super::game::{KillAnnounced, ShipFreqChange};

/// Most flags an arena can have.
pub const MAX_FLAGS: usize = 255;
/// Turf game: flags stay on the map.
pub const CARRY_NONE: i32 = 0;
/// Players carry any number of flags.
pub const CARRY_ALL: i32 = 1;
/// Ticks between turf ownership broadcasts.
const TURF_UPDATE_DELAY: u64 = 3000;

/// Where a flag is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlagState {
    /// Not in play.
    #[default]
    None,
    /// Carried by a player.
    Carried,
    /// Lying on the map.
    OnMap,
}

/// One flag (ASSS `FlagInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagInfo {
    /// State.
    pub state: FlagState,
    /// Carrier, when carried.
    pub carrier: Option<PlayerId>,
    /// Tile x (on map), or where it was.
    pub x: i16,
    /// Tile y.
    pub y: i16,
    /// Owning frequency (-1 = neutral).
    pub freq: i16,
}

impl Default for FlagInfo {
    fn default() -> Self {
        Self {
            state: FlagState::None,
            carrier: None,
            x: -1,
            y: -1,
            freq: -1,
        }
    }
}

impl FlagInfo {
    /// A flag on the map at `(x, y)` owned by `freq`.
    pub fn on_map(x: i16, y: i16, freq: i16) -> Self {
        Self {
            state: FlagState::OnMap,
            carrier: None,
            x,
            y,
            freq,
        }
    }

    /// A flag carried by `player`.
    pub fn carried(player: PlayerId) -> Self {
        Self {
            state: FlagState::Carried,
            carrier: Some(player),
            ..Self::default()
        }
    }
}

/// How a player gained a flag (ASSS `FLAGGAIN_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainHow {
    /// Picked up from the map.
    Pickup,
    /// Took it from a carrier they killed.
    Kill,
    /// Anything else.
    Other,
}

/// Why a carrier lost a flag (ASSS `CLEANUP_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// Dropped normally (flag timer).
    Dropped,
    /// Dropped in a safe zone.
    InSafe,
    /// Changed ship.
    ShipChange,
    /// Changed frequency.
    FreqChange,
    /// Left the arena.
    LeftArena,
    /// Killed; flags went to the killer.
    KillNormal,
    /// Team-killed.
    KillTk,
    /// Killed, but the killer couldn't carry more.
    KillCantCarry,
    /// Killed by a fake player.
    KillFake,
    /// Anything else.
    Other,
}

/// The rules of a flag game, attached per arena (ASSS `Iflaggame`).
pub trait FlagGame: Send + Sync {
    /// The arena was created: set the carry mode, reserve and place flags.
    fn init(&self, zone: &mut Zone, arena: ArenaId);
    /// A player touched flag `fid`, which is on the map.
    fn flag_touch(&self, zone: &mut Zone, arena: ArenaId, player: PlayerId, fid: usize);
    /// Flag `fid` left `carrier` (whose frequency was `freq`) for `reason`;
    /// the game decides where it goes. The flag is in state `None`.
    fn cleanup(
        &self,
        zone: &mut Zone,
        arena: ArenaId,
        fid: usize,
        reason: Cleanup,
        carrier: PlayerId,
        freq: i16,
    );
}

/// ASSS `CB_FLAGGAIN`.
#[derive(Debug, Clone, Copy)]
pub struct FlagGain {
    /// Arena.
    pub arena: ArenaId,
    /// Player.
    pub player: PlayerId,
    /// Flag id.
    pub fid: usize,
    /// How.
    pub how: GainHow,
}

impl Event for FlagGain {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_FLAGLOST`.
#[derive(Debug, Clone, Copy)]
pub struct FlagLost {
    /// Arena.
    pub arena: ArenaId,
    /// Former carrier.
    pub player: PlayerId,
    /// Flag id.
    pub fid: usize,
    /// Why.
    pub reason: Cleanup,
}

impl Event for FlagLost {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_FLAGONMAP`: a flag was placed (or a turf flag changed owner).
#[derive(Debug, Clone, Copy)]
pub struct FlagOnMap {
    /// Arena.
    pub arena: ArenaId,
    /// Flag id.
    pub fid: usize,
    /// Tile x (-1 for turf flags).
    pub x: i16,
    /// Tile y.
    pub y: i16,
    /// Owner.
    pub freq: i16,
}

impl Event for FlagOnMap {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_FLAGRESET`: the flag game ended (`freq` won `points`; -1 for a
/// plain reset).
#[derive(Debug, Clone, Copy)]
pub struct FlagReset {
    /// Arena.
    pub arena: ArenaId,
    /// Winning frequency, or -1.
    pub freq: i16,
    /// Points.
    pub points: i32,
}

impl Event for FlagReset {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

#[derive(Default)]
struct ArenaFlags {
    game: Option<Arc<dyn FlagGame>>,
    carry_mode: Option<i32>,
    flags: Vec<FlagInfo>,
    toucher: Option<PlayerId>,
    during_init: bool,
}

fn data(zone: &mut Zone, a: ArenaId) -> Option<&mut ArenaFlags> {
    zone.arenas
        .get_mut(a)
        .map(|a| a.ext.get_or_default::<ArenaFlags>())
}

fn data_ref(zone: &Zone, a: ArenaId) -> Option<&ArenaFlags> {
    zone.arenas.get(a)?.ext.get::<ArenaFlags>()
}

fn active(zone: &Zone, a: ArenaId) -> bool {
    data_ref(zone, a).is_some_and(|d| d.game.is_some() && d.carry_mode.is_some())
}

fn send_arena<P: Packet>(zone: &mut Zone, a: ArenaId, p: &P) {
    let mut buf = Vec::with_capacity(16);
    p.encode(&mut buf);
    zone.send_to_arena(a, None, &buf, SendFlags::RELIABLE);
}

fn adjust_carried(zone: &mut Zone, p: PlayerId, delta: i16) {
    if let Some(pl) = zone.players.get_mut(p) {
        pl.flags_carried = (pl.flags_carried + delta).max(0);
    }
}

fn turf_packet(zone: &Zone, a: ArenaId) -> Vec<u8> {
    let owners = data_ref(zone, a)
        .map(|d| d.flags.iter().map(|f| f.freq).collect())
        .unwrap_or_default();
    let mut buf = Vec::new();
    TurfFlags { owners }.encode(&mut buf);
    buf
}

fn send_turf(zone: &mut Zone, a: ArenaId) {
    let buf = turf_packet(zone, a);
    zone.send_to_arena(a, None, &buf, SendFlags::UNRELIABLE);
}

/// Number of flag slots in an arena.
pub fn flag_count(zone: &Zone, a: ArenaId) -> usize {
    data_ref(zone, a).map_or(0, |d| d.flags.len())
}

/// The state of flag `fid`.
pub fn get_flag(zone: &Zone, a: ArenaId, fid: usize) -> Option<FlagInfo> {
    data_ref(zone, a)
        .filter(|_| active(zone, a))
        .and_then(|d| d.flags.get(fid).copied())
}

/// The arena's carry mode, once its flag game has set it.
pub fn carry_mode(zone: &Zone, a: ArenaId) -> Option<i32> {
    data_ref(zone, a).and_then(|d| d.carry_mode)
}

/// Sets the carry mode; only allowed from [`FlagGame::init`].
pub fn set_carry_mode(zone: &mut Zone, a: ArenaId, mode: i32) {
    let Some(d) = data(zone, a) else { return };
    if d.during_init {
        d.carry_mode = Some(mode);
    } else {
        tracing::error!(target: "flagcore", arena = a.0, "set_carry_mode called after initialization");
    }
}

/// Makes room for `count` flags.
pub fn reserve_flags(zone: &mut Zone, a: ArenaId, count: usize) {
    if count >= MAX_FLAGS {
        return;
    }
    if let Some(d) = data(zone, a)
        && d.flags.len() < count
    {
        d.flags.resize(count, FlagInfo::default());
    }
}

/// Flags owned by (on the map) or carried by members of `freq`.
pub fn count_freq_flags(zone: &Zone, a: ArenaId, freq: i16) -> usize {
    if !active(zone, a) {
        return 0;
    }
    let Some(d) = data_ref(zone, a) else { return 0 };
    d.flags
        .iter()
        .filter(|f| match f.state {
            FlagState::OnMap => f.freq == freq,
            FlagState::Carried => f
                .carrier
                .and_then(|c| zone.players.get(c))
                .is_some_and(|c| c.freq as i16 == freq),
            FlagState::None => false,
        })
        .count()
}

/// ASSS `SetFlags` for one flag: moves flag `fid` to `nfi`, telling clients.
pub fn set_flag(zone: &mut Zone, a: ArenaId, fid: usize, mut nfi: FlagInfo) {
    if fid >= MAX_FLAGS || !active(zone, a) {
        return;
    }
    let Some(d) = data(zone, a) else { return };
    if d.flags.len() <= fid {
        d.flags.resize(fid + 1, FlagInfo::default());
    }
    let ofi = d.flags[fid];
    let (mode, toucher, during_init) = (d.carry_mode.unwrap_or(-1), d.toucher, d.during_init);

    if mode == CARRY_NONE {
        if nfi.state != FlagState::OnMap {
            tracing::error!(target: "flagcore", arena = a.0, "asked to take turf flag off map");
            return;
        }
        nfi.x = -1;
        nfi.y = -1;
        nfi.carrier = None;
        d.flags[fid] = nfi;
        if let Some(t) = toucher {
            send_arena(
                zone,
                a,
                &FlagPickup {
                    flag_id: fid as i16,
                    pid: t.0 as i16,
                },
            );
        } else if !during_init {
            send_turf(zone, a);
        }
        zone.fire(&FlagOnMap {
            arena: a,
            fid,
            x: -1,
            y: -1,
            freq: nfi.freq,
        });
        return;
    }

    if nfi.state == FlagState::Carried && nfi.carrier.is_none() {
        tracing::error!(target: "flagcore", arena = a.0, "asked to set carried flag with no carrier");
        return;
    }
    if nfi.state != FlagState::Carried && nfi.carrier.is_some() {
        tracing::error!(target: "flagcore", arena = a.0, "asked to set non-carried flag with carrier");
        return;
    }
    if let Some(c) = nfi.carrier {
        nfi.freq = zone.players.get(c).map_or(-1, |p| p.freq as i16);
    }

    let mut ostate = ofi.state;
    if ofi.state == FlagState::Carried {
        if nfi.state == FlagState::Carried && nfi.carrier == ofi.carrier {
            return;
        }
        // the client can't drop one flag: drop them all, then re-pick-up
        // the others
        let Some(carrier) = ofi.carrier else { return };
        send_arena(
            zone,
            a,
            &FlagDrop {
                pid: carrier.0 as i16,
            },
        );
        if let Some(p) = zone.players.get_mut(carrier) {
            p.flags_carried = 0;
        }
        let Some(d) = data(zone, a) else { return };
        d.flags[fid] = FlagInfo::default();
        let mine: Vec<usize> = d
            .flags
            .iter()
            .enumerate()
            .filter(|(_, f)| f.state == FlagState::Carried && f.carrier == Some(carrier))
            .map(|(i, _)| i)
            .collect();
        for i in &mine {
            send_arena(
                zone,
                a,
                &FlagPickup {
                    flag_id: *i as i16,
                    pid: carrier.0 as i16,
                },
            );
            adjust_carried(zone, carrier, 1);
        }
        if !mine.is_empty() {
            tracing::warn!(target: "flagcore", arena = a.0, fid, "faked removing one carried flag");
        }
        zone.fire(&FlagLost {
            arena: a,
            player: carrier,
            fid,
            reason: Cleanup::Other,
        });
        ostate = FlagState::None;
    }

    let store = |zone: &mut Zone, f: FlagInfo| {
        if let Some(d) = data(zone, a) {
            d.flags[fid] = f;
        }
    };
    match (ostate, nfi.state) {
        (FlagState::None, FlagState::None) => store(zone, nfi),
        (FlagState::None | FlagState::OnMap, FlagState::Carried) => {
            let Some(c) = nfi.carrier else { return };
            send_arena(
                zone,
                a,
                &FlagPickup {
                    flag_id: fid as i16,
                    pid: c.0 as i16,
                },
            );
            adjust_carried(zone, c, 1);
            store(zone, nfi);
            let how = if ostate == FlagState::OnMap {
                GainHow::Pickup
            } else {
                GainHow::Other
            };
            zone.fire(&FlagGain {
                arena: a,
                player: c,
                fid,
                how,
            });
        }
        (FlagState::None | FlagState::OnMap, FlagState::OnMap) => {
            send_arena(
                zone,
                a,
                &FlagLocation {
                    flag_id: fid as u16,
                    x: nfi.x as u16,
                    y: nfi.y as u16,
                    freq: nfi.freq as u16,
                },
            );
            store(zone, nfi);
            zone.fire(&FlagOnMap {
                arena: a,
                fid,
                x: nfi.x,
                y: nfi.y,
                freq: nfi.freq,
            });
        }
        (FlagState::OnMap, FlagState::None) => {
            // fake a removal by moving it off the playable area
            send_arena(
                zone,
                a,
                &FlagLocation {
                    flag_id: fid as u16,
                    x: 0xFFFF,
                    y: 0xFFFF,
                    freq: 0xFFFF,
                },
            );
            store(zone, FlagInfo::default());
            tracing::warn!(target: "flagcore", arena = a.0, fid, "faked removing flag");
        }
        (FlagState::Carried, _) => {}
    }
}

/// Drops every flag `player` carries and lets the game place them.
fn cleanup(zone: &mut Zone, a: ArenaId, player: PlayerId, reason: Cleanup) {
    if !active(zone, a) {
        return;
    }
    let Some(d) = data(zone, a) else { return };
    let mut lost = Vec::new();
    for (i, f) in d.flags.iter_mut().enumerate() {
        if f.state == FlagState::Carried && f.carrier == Some(player) {
            f.state = FlagState::None;
            f.carrier = None;
            // keep freq for the game's use
            lost.push((i, f.freq));
        }
    }
    let game = d.game.clone();
    for (fid, freq) in lost {
        adjust_carried(zone, player, -1);
        zone.fire(&FlagLost {
            arena: a,
            player,
            fid,
            reason,
        });
        if let Some(g) = &game {
            g.cleanup(zone, a, fid, reason, player, freq);
        }
    }
}

/// ASSS `FlagReset`: ends the game (`freq` won `points`, or -1 for none).
pub fn flag_reset(zone: &mut Zone, a: ArenaId, freq: i16, points: i32) {
    if !active(zone, a) {
        return;
    }
    let mode = carry_mode(zone, a);
    if mode == Some(CARRY_NONE) {
        if let Some(d) = data(zone, a) {
            d.flags.iter_mut().for_each(|f| f.freq = -1);
        }
        send_turf(zone, a);
        tracing::info!(target: "flagcore", arena = a.0, "turf game reset");
    } else {
        let carriers: Vec<PlayerId> = data(zone, a)
            .map(|d| {
                let c = d.flags.iter().filter_map(|f| f.carrier).collect();
                d.flags.iter_mut().for_each(|f| *f = FlagInfo::default());
                c
            })
            .unwrap_or_default();
        for c in carriers {
            adjust_carried(zone, c, -1);
        }
        send_arena(zone, a, &FlagVictory { freq, points });
        if freq == -1 {
            tracing::info!(target: "flagcore", arena = a.0, "flag reset");
        } else {
            tracing::info!(target: "flagcore", arena = a.0, freq, points, "flag victory");
        }
    }
    zone.fire(&FlagReset {
        arena: a,
        freq,
        points,
    });
    // after the callbacks, so rewards land in the game that just ended
    if mode != Some(CARRY_NONE) {
        super::persist::end_interval(zone, Some(a), super::persist::Interval::Game);
    }
}

fn on_kill(zone: &mut Zone, k: &KillAnnounced) {
    let a = k.arena;
    if !active(zone, a) {
        return;
    }
    let killed_flags = zone.players.get(k.killed).map_or(0, |p| p.flags_carried);
    if killed_flags == 0 {
        return;
    }
    let Some(mode) = carry_mode(zone, a) else {
        return;
    };
    if mode == CARRY_NONE {
        tracing::warn!(target: "flagcore", pid = k.killed.0, "got flag kill in turf game");
        return;
    }
    let can_carry = if mode == CARRY_ALL {
        MAX_FLAGS as i16
    } else {
        (mode - 1) as i16
    };
    let (killer_fake, killer_freq) = zone
        .players
        .get(k.killer)
        .map_or((false, 0), |p| (p.kind == ClientKind::Fake, p.freq));
    let killed_freq = zone.players.get(k.killed).map_or(0, |p| p.freq);
    let reason = if killer_fake {
        Cleanup::KillFake
    } else if killer_freq == killed_freq {
        Cleanup::KillTk
    } else {
        Cleanup::KillNormal
    };
    let game = data_ref(zone, a).and_then(|d| d.game.clone());
    let fids: Vec<usize> = data_ref(zone, a)
        .map(|d| {
            d.flags
                .iter()
                .enumerate()
                .filter(|(_, f)| f.state == FlagState::Carried && f.carrier == Some(k.killed))
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default();
    let mut overflow = false;
    for fid in fids {
        let killer_has = zone.players.get(k.killer).map_or(0, |p| p.flags_carried);
        if killer_has < can_carry {
            adjust_carried(zone, k.killer, 1);
            adjust_carried(zone, k.killed, -1);
            if let Some(d) = data(zone, a) {
                d.flags[fid].carrier = Some(k.killer);
                d.flags[fid].freq = killer_freq as i16;
            }
            zone.fire(&FlagLost {
                arena: a,
                player: k.killed,
                fid,
                reason,
            });
            zone.fire(&FlagGain {
                arena: a,
                player: k.killer,
                fid,
                how: GainHow::Kill,
            });
            if let Some(g) = &game {
                g.cleanup(zone, a, fid, reason, k.killed, killed_freq as i16);
            }
        } else {
            overflow = true;
        }
    }
    if overflow {
        // the clients think the killer got everything: drop all, re-pick-up
        // the killer's own, and void the rest
        send_arena(
            zone,
            a,
            &FlagDrop {
                pid: k.killer.0 as i16,
            },
        );
        let all: Vec<(usize, Option<PlayerId>)> = data_ref(zone, a)
            .map(|d| {
                d.flags
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (i, f.carrier))
                    .collect()
            })
            .unwrap_or_default();
        for (fid, carrier) in all {
            if carrier == Some(k.killer) {
                send_arena(
                    zone,
                    a,
                    &FlagPickup {
                        flag_id: fid as i16,
                        pid: k.killer.0 as i16,
                    },
                );
            } else if carrier == Some(k.killed) {
                adjust_carried(zone, k.killed, -1);
                if let Some(d) = data(zone, a) {
                    d.flags[fid].state = FlagState::None;
                    d.flags[fid].carrier = None;
                }
                zone.fire(&FlagLost {
                    arena: a,
                    player: k.killed,
                    fid,
                    reason: Cleanup::KillCantCarry,
                });
                if let Some(g) = &game {
                    g.cleanup(
                        zone,
                        a,
                        fid,
                        Cleanup::KillCantCarry,
                        k.killed,
                        killed_freq as i16,
                    );
                }
            }
        }
    }
}

fn on_touch(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 3 {
        tracing::warn!(target: "flagcore", pid = pid.0, len = pkt.len(), "bad flag pickup packet length");
        return;
    }
    let Ok(req) = c2s::PickupFlag::decode(pkt) else {
        return;
    };
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(a), PlayerStatus::Playing) = (p.arena, p.status) else {
        tracing::warn!(target: "flagcore", pid = pid.0, "flag pickup from bad state");
        return;
    };
    if p.ship.is_spec() {
        tracing::warn!(target: "flagcore", pid = pid.0, "flag pickup packet from spec");
        return;
    }
    if super::lagdata::no_flags_balls(zone, pid) {
        tracing::debug!(target: "flagcore", pid = pid.0, "too lagged to pick up a flag");
        return;
    }
    let Ok(fid) = usize::try_from(req.flag_id) else {
        return;
    };
    if !active(zone, a) || get_flag(zone, a, fid).is_none_or(|f| f.state != FlagState::OnMap) {
        tracing::warn!(target: "flagcore", pid = pid.0, fid, "tried to pick up flag not on map");
        return;
    }
    let Some(game) = data_ref(zone, a).and_then(|d| d.game.clone()) else {
        return;
    };
    if let Some(d) = data(zone, a) {
        d.toucher = Some(pid);
    }
    game.flag_touch(zone, a, pid, fid);
    if let Some(d) = data(zone, a) {
        d.toucher = None;
    }
}

fn on_drop(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 1 {
        tracing::warn!(target: "flagcore", pid = pid.0, len = pkt.len(), "bad flag drop packet length");
        return;
    }
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(a), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    if p.ship.is_spec() {
        return;
    }
    let in_safe = p.pos.status & oss_proto::position::status::SAFEZONE != 0;
    if carry_mode(zone, a) == Some(CARRY_NONE) {
        tracing::warn!(target: "flagcore", pid = pid.0, "flag drop packet in turf game");
        return;
    }
    send_arena(zone, a, &FlagDrop { pid: pid.0 as i16 });
    cleanup(
        zone,
        a,
        pid,
        if in_safe {
            Cleanup::InSafe
        } else {
            Cleanup::Dropped
        },
    );
}

/// The `flagcore` module.
pub struct FlagCoreModule;

impl Module for FlagCoreModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::PICKUP_FLAG, on_touch);
        zone.on_packet(me, c2s::kind::DROP_FLAGS, on_drop);
        zone.on::<super::arenaman::ArenaAction>(me, Scope::Global, move |z, ev| {
            use super::arenaman::ArenaActionKind;
            match ev.action {
                ArenaActionKind::Create => {
                    let Some(game) = z.iface_in::<dyn FlagGame>(ev.arena) else { return };
                    if let Some(a) = z.arenas.get_mut(ev.arena) {
                        a.ext.insert(ArenaFlags { game: Some(game.clone()), during_init: true, ..Default::default() });
                    }
                    game.init(z, ev.arena);
                    let mode = data(z, ev.arena).and_then(|d| {
                        d.during_init = false;
                        d.carry_mode
                    });
                    match mode {
                        Some(m) => {
                            if let (Some(cs), Some(k)) =
                                (z.iface::<dyn ClientSettings>(), OverrideKey::lookup("Flag", "CarryFlags"))
                            {
                                cs.arena_override(z, ev.arena, k, m);
                            }
                            if m == CARRY_NONE {
                                let a = ev.arena;
                                z.set_timer(me, TURF_UPDATE_DELAY, TURF_UPDATE_DELAY, move |z| {
                                    if z.arenas.get(a).is_none() {
                                        return false;
                                    }
                                    send_turf(z, a);
                                    true
                                });
                            }
                        }
                        None => {
                            tracing::error!(target: "flagcore", arena = ev.arena.0, "flag game didn't set carry mode");
                            if let Some(a) = z.arenas.get_mut(ev.arena) {
                                a.ext.remove::<ArenaFlags>();
                            }
                        }
                    }
                }
                ArenaActionKind::Destroy => {
                    if let Some(a) = z.arenas.get_mut(ev.arena) {
                        a.ext.remove::<ArenaFlags>();
                    }
                }
                _ => {}
            }
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            let Some(a) = ev.arena else { return };
            match ev.action {
                Action::EnterArena => {
                    if let Some(p) = z.players.get_mut(ev.player) {
                        p.flags_carried = 0;
                    }
                    if !active(z, a) {
                        return;
                    }
                    if carry_mode(z, a) == Some(CARRY_NONE) {
                        let buf = turf_packet(z, a);
                        z.send(ev.player, &buf, SendFlags::RELIABLE);
                    } else {
                        let on_map: Vec<(usize, FlagInfo)> = data_ref(z, a)
                            .map(|d| {
                                d.flags
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, f)| f.state == FlagState::OnMap)
                                    .map(|(i, f)| (i, *f))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let mut buf = Vec::with_capacity(9);
                        for (fid, f) in on_map {
                            buf.clear();
                            FlagLocation {
                                flag_id: fid as u16,
                                x: f.x as u16,
                                y: f.y as u16,
                                freq: f.freq as u16,
                            }
                            .encode(&mut buf);
                            z.send(ev.player, &buf, SendFlags::RELIABLE);
                        }
                    }
                }
                Action::LeaveArena => cleanup(z, a, ev.player, Cleanup::LeftArena),
                _ => {}
            }
        });
        zone.on::<NewPlayer>(me, Scope::Global, |z, ev| {
            let Some(p) = z.players.get(ev.player) else {
                return;
            };
            if p.kind == ClientKind::Fake
                && !ev.created
                && let Some(a) = p.arena
            {
                cleanup(z, a, ev.player, Cleanup::LeftArena);
            }
        });
        zone.on::<ShipFreqChange>(me, Scope::Global, |z, ev| {
            let reason = if ev.ship_change {
                Cleanup::ShipChange
            } else {
                Cleanup::FreqChange
            };
            cleanup(z, ev.arena, ev.player, reason);
        });
        zone.on::<KillAnnounced>(me, Scope::Global, on_kill);
        Ok(())
    }
}
