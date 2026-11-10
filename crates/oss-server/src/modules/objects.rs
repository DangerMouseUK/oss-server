//! `objects`: LVZ object control (ASSS `objects.c`).
//!
//! An arena's `.lvz` files declare objects — map objects pinned to a tile
//! position, screen objects pinned to a corner of the HUD — each with an
//! image, a display layer, a display time and a mode. Objects in
//! `ServerControlled` mode show nothing until the server says so, which is
//! what every scoreboard, countdown and lit-up base in a zone is built from.
//!
//! The server reads the object definitions out of the same `.lvz` files it
//! serves for download and keeps, per arena, each object's defaults beside
//! its current state. Objects start hidden. `?objon`/`?objoff`/`?objset`
//! show and hide them (S2C `0x35`); `?objmove`, `?objimage`, `?objlayer`,
//! `?objtimer` and `?objmode` change the other properties (S2C `0x36`).
//! `?objinfo` and `?objlist` report what an arena has. Aimed at the arena,
//! a change is remembered and replayed to anyone arriving later, so a
//! late-joiner sees what everyone else sees; aimed at one player or one
//! frequency it is sent and forgotten, as in ASSS.
//!
//! Bots drive the same two packets through C2S `0x0A`, which this module
//! handles: a player with the `broadcastbot` capability may relay object
//! packets, one with `broadcastany` may relay anything. Arena-wide object
//! packets from a bot update the remembered state too.
//!
//! Deliberate divergences from ASSS 1.4.4:
//!
//! 1. **`?objset` parses every item.** ASSS's parser stops one character
//!    short of each sign after the first, so `?objset +1 -2` reads the
//!    second item as "turn off object 32766" — the sign is lost and the id
//!    comes out negative. Here `[+-]<id>` is read as written, for every item.
//! 2. **The property commands work on any target.** ASSS's `?objmove` and
//!    the rest return early unless the target is the arena, although their
//!    help text says "Targets: any". Here the record is still read from the
//!    arena's state, so a private `?objimage` shows that player a different
//!    image without changing the arena.
//! 3. **A toggle's off bit is read from bit 15.** ASSS writes it there but
//!    reads it back through a union whose `off` bitfield lands on bit 0, so
//!    a bot's arena-wide `0x35` is recorded by the low bit of the object id.
//!    This reads the bit that was written.
//! 4. **An unparseable id is refused** with a message rather than silently
//!    read as object 0, which is what ASSS's `atoi` does.
//!
//! Loading is capped at [`MAX_OBJECTS`] objects per arena: ids are 15 bits,
//! so more than that cannot be addressed anyway, and a malformed `.lvz`
//! should not be able to claim the server's memory.

use std::sync::Arc;

use oss_formats::lvz::{Lvz, ObjectSection};
use oss_proto::Packet;
use oss_proto::c2s::{self, Rebroadcast};
use oss_proto::common::{ObjectChange, ObjectToggle, object_change};
use oss_proto::objects::{ObjectData, mode, reference};
use oss_proto::s2c::{self, MoveObjects, ToggleObjects};

use crate::arena::ArenaId;
use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};
use super::capman::Capabilities;
use super::chat::message;
use super::core::{Action, PlayerAction};

/// Lets a player relay LVZ object packets to the arena (ASSS
/// `CAP_BROADCAST_BOT`).
pub const CAP_BROADCAST_BOT: &str = "broadcastbot";
/// Lets a player relay any packet at all (ASSS `CAP_BROADCAST_ANY`).
pub const CAP_BROADCAST_ANY: &str = "broadcastany";

/// Most objects one arena will load. Object ids are 15 bits wide, so no
/// `.lvz` can usefully declare more.
pub const MAX_OBJECTS: usize = 32768;

/// ASSS sends "everything changed" in a state replay rather than working out
/// a mask; the client reads only the fields the mask names, so the spare
/// bits are harmless.
const MASK_ALL: u8 = 0xFF;

/// Where an object goes. For a map object the coordinates are pixels on the
/// map and the reference points are ignored; for a screen object they are
/// offsets from [`oss_proto::objects::reference`] points on the HUD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Place {
    /// Horizontal position.
    pub x: i16,
    /// Vertical position.
    pub y: i16,
    /// Screen reference point the x offset is measured from.
    pub x_ref: u8,
    /// Screen reference point the y offset is measured from.
    pub y_ref: u8,
}

// ---- state ---------------------------------------------------------------

/// What a player may relay through C2S `0x0A`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Relay {
    /// Nothing.
    #[default]
    None,
    /// Object packets only.
    Objects,
    /// Any packet.
    Any,
}

/// Per-player: what they may relay (ASSS `podata.brallow`).
#[derive(Debug, Clone, Copy, Default)]
struct Allow(Relay);

/// One object an arena's `.lvz` files declared.
#[derive(Debug, Clone, Copy)]
struct LvzObject {
    /// Hidden. Objects start hidden, as in ASSS.
    off: bool,
    /// As the file declared it.
    defaults: ObjectData,
    /// As the arena has it now.
    current: ObjectData,
}

/// Per-arena: every object, in file order (ASSS `aodata.list`).
#[derive(Debug, Default)]
struct ArenaObjects {
    list: Vec<LvzObject>,
}

fn objects_of(zone: &Zone, arena: ArenaId) -> Option<&ArenaObjects> {
    zone.arenas.get(arena)?.ext.get::<ArenaObjects>()
}

/// ASSS `getDataFromId`: the first object with this id.
fn object(zone: &Zone, arena: ArenaId, id: u16) -> Option<&LvzObject> {
    objects_of(zone, arena)?
        .list
        .iter()
        .find(|o| o.defaults.id() == id)
}

fn object_mut(zone: &mut Zone, arena: ArenaId, id: u16) -> Option<&mut LvzObject> {
    zone.arenas
        .get_mut(arena)?
        .ext
        .get_mut::<ArenaObjects>()?
        .list
        .iter_mut()
        .find(|o| o.defaults.id() == id)
}

/// The arena a target names: its own, a frequency's, or the player's.
fn arena_of(zone: &Zone, t: Target) -> Option<ArenaId> {
    match t {
        Target::Arena(a) | Target::Freq(a, _) => Some(a),
        Target::Player(p) => zone.players.get(p).and_then(|x| x.arena),
        Target::None => None,
    }
}

/// ASSS `SendToTarget`.
fn send_target(zone: &mut Zone, t: Target, data: &[u8]) {
    match t {
        Target::Arena(a) => zone.send_to_arena(a, None, data, SendFlags::RELIABLE),
        Target::Freq(a, f) => {
            let ids: Vec<PlayerId> = zone
                .players
                .in_arena(a)
                .filter(|p| p.freq == f)
                .map(|p| p.id)
                .collect();
            zone.send_many(ids, data, SendFlags::RELIABLE);
        }
        Target::Player(p) => zone.send(p, data, SendFlags::RELIABLE),
        Target::None => {}
    }
}

// ---- loading -------------------------------------------------------------

/// Reads the object sections of every `.lvz` file the arena serves.
fn load_objects(zone: &mut Zone, arena: ArenaId) {
    let mut list: Vec<LvzObject> = Vec::new();
    for (path, _optional) in super::mapnewsdl::lvz_files(zone, arena) {
        let Some(data) = zone.cfg.source().read(&path) else {
            tracing::warn!(target: "objects", arena = arena.0, file = %path.display(), "can't read LVZ file");
            continue;
        };
        let lvz = match Lvz::parse(&data) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(target: "objects", arena = arena.0, file = %path.display(), error = %e, "bad LVZ file");
                continue;
            }
        };
        for section in lvz.sections.iter().filter(|s| s.is_object_section()) {
            let objects = match section.data().map(|raw| ObjectSection::parse(&raw)) {
                Ok(Ok(s)) => s.objects,
                Ok(Err(e)) => {
                    tracing::warn!(target: "objects", arena = arena.0, file = %path.display(), error = %e, "bad LVZ object section");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(target: "objects", arena = arena.0, file = %path.display(), error = %e, "can't inflate LVZ section");
                    continue;
                }
            };
            for o in objects {
                if list.len() >= MAX_OBJECTS {
                    tracing::warn!(target: "objects", arena = arena.0, max = MAX_OBJECTS, "too many LVZ objects, ignoring the rest");
                    break;
                }
                list.push(LvzObject {
                    off: true,
                    defaults: o,
                    current: o,
                });
            }
        }
    }
    if !list.is_empty() {
        tracing::debug!(target: "objects", arena = arena.0, objects = list.len(), "loaded LVZ objects");
    }
    if let Some(a) = zone.arenas.get_mut(arena) {
        a.ext.insert(ArenaObjects { list });
    }
}

// ---- the interface -------------------------------------------------------

/// Drives LVZ objects (ASSS `Iobjects`). Every call takes a [`Target`];
/// only [`Target::Arena`] changes what a late-joiner will be sent.
pub trait Objects: Send + Sync {
    /// Sends a player everything the arena has changed since the defaults.
    fn send_state(&self, zone: &mut Zone, player: PlayerId);
    /// Shows or hides one object.
    fn toggle(&self, zone: &mut Zone, target: Target, id: u16, on: bool);
    /// Shows and hides several objects in one packet.
    fn toggle_set(&self, zone: &mut Zone, target: Target, items: &[(u16, bool)]);
    /// Moves an object.
    fn move_object(&self, zone: &mut Zone, target: Target, id: u16, to: Place);
    /// Changes an object's image.
    fn image(&self, zone: &mut Zone, target: Target, id: u16, image: u8);
    /// Changes an object's layer ([`oss_proto::objects::layer`]).
    fn layer(&self, zone: &mut Zone, target: Target, id: u16, layer: u8);
    /// Changes an object's display time, in 1/10 s (0 = until hidden).
    fn timer(&self, zone: &mut Zone, target: Target, id: u16, time: u16);
    /// Changes an object's mode ([`oss_proto::objects::mode`]).
    fn mode(&self, zone: &mut Zone, target: Target, id: u16, mode: u8);
}

struct ObjectsImpl;

impl Objects for ObjectsImpl {
    fn send_state(&self, zone: &mut Zone, player: PlayerId) {
        send_state(zone, player);
    }
    fn toggle(&self, zone: &mut Zone, target: Target, id: u16, on: bool) {
        toggle(zone, target, id, on);
    }
    fn toggle_set(&self, zone: &mut Zone, target: Target, items: &[(u16, bool)]) {
        toggle_set(zone, target, items);
    }
    fn move_object(&self, zone: &mut Zone, target: Target, id: u16, to: Place) {
        move_object(zone, target, id, to);
    }
    fn image(&self, zone: &mut Zone, target: Target, id: u16, image: u8) {
        set_image(zone, target, id, image);
    }
    fn layer(&self, zone: &mut Zone, target: Target, id: u16, layer: u8) {
        set_layer(zone, target, id, layer);
    }
    fn timer(&self, zone: &mut Zone, target: Target, id: u16, time: u16) {
        set_timer(zone, target, id, time);
    }
    fn mode(&self, zone: &mut Zone, target: Target, id: u16, mode: u8) {
        set_mode(zone, target, id, mode);
    }
}

// ---- sending -------------------------------------------------------------

/// ASSS `SendState`: the arena's object state, for someone who just arrived.
pub fn send_state(zone: &mut Zone, player: PlayerId) {
    let Some(arena) = zone.players.get(player).and_then(|p| p.arena) else {
        return;
    };
    let (toggles, changes) = {
        let Some(objs) = objects_of(zone, arena) else {
            return;
        };
        let mut toggles = Vec::new();
        let mut changes = Vec::new();
        for o in &objs.list {
            if o.current != o.defaults {
                changes.push(ObjectChange {
                    mask: MASK_ALL,
                    data: o.current,
                });
            }
            if !o.off {
                toggles.push(ObjectToggle::new(o.defaults.id(), true));
            }
        }
        (toggles, changes)
    };
    if !toggles.is_empty() {
        let mut buf = Vec::with_capacity(1 + 2 * toggles.len());
        ToggleObjects { toggles }.encode(&mut buf);
        zone.send(player, &buf, SendFlags::RELIABLE);
    }
    if !changes.is_empty() {
        let mut buf = Vec::with_capacity(1 + 11 * changes.len());
        MoveObjects { changes }.encode(&mut buf);
        zone.send(player, &buf, SendFlags::RELIABLE);
    }
}

/// Remembers a toggle for the arena. An object with a display time hides
/// itself, so ASSS does not record one as shown; neither does this.
fn remember_toggle(zone: &mut Zone, arena: ArenaId, id: u16, on: bool) {
    if let Some(o) = object_mut(zone, arena, id)
        && o.current.display_time() == 0
    {
        o.off = !on;
    }
}

/// ASSS `Toggle`.
pub fn toggle(zone: &mut Zone, target: Target, id: u16, on: bool) {
    toggle_set(zone, target, &[(id, on)]);
}

/// ASSS `ToggleSet`: several objects in one packet.
pub fn toggle_set(zone: &mut Zone, target: Target, items: &[(u16, bool)]) {
    if items.is_empty() {
        return;
    }
    let toggles = items
        .iter()
        .map(|&(id, on)| ObjectToggle::new(id, on))
        .collect();
    let mut buf = Vec::with_capacity(1 + 2 * items.len());
    ToggleObjects { toggles }.encode(&mut buf);
    send_target(zone, target, &buf);
    if let Target::Arena(a) = target {
        for &(id, on) in items {
            remember_toggle(zone, a, id, on);
        }
    }
}

/// The body of ASSS's `BEGIN_EXTENDED`/`END_EXTENDED` pair: read the object's
/// current record, change one field, send it, and remember it if the whole
/// arena was told.
fn extended(
    zone: &mut Zone,
    target: Target,
    id: u16,
    mask: u8,
    apply: impl FnOnce(&mut ObjectData, &ObjectData),
) {
    let Some(arena) = arena_of(zone, target) else {
        return;
    };
    let Some(o) = object(zone, arena, id) else {
        return;
    };
    let defaults = o.defaults;
    let mut next = o.current;
    apply(&mut next, &defaults);
    let mut buf = Vec::with_capacity(1 + 11);
    MoveObjects {
        changes: vec![ObjectChange { mask, data: next }],
    }
    .encode(&mut buf);
    send_target(zone, target, &buf);
    if let Target::Arena(a) = target
        && let Some(o) = object_mut(zone, a, id)
    {
        o.current = next;
    }
}

/// ASSS `Move`. Whether the coordinates are tile pixels or screen offsets is
/// decided by what the file declared, not by the caller.
pub fn move_object(zone: &mut Zone, target: Target, id: u16, to: Place) {
    extended(
        zone,
        target,
        id,
        object_change::POSITION,
        |next, defaults| {
            let packed = if defaults.is_map_object() {
                ObjectData::map(id, to.x, to.y)
            } else {
                ObjectData::screen(id, to.x_ref, to.x, to.y_ref, to.y)
            };
            next.x_word = packed.x_word;
            next.y_word = packed.y_word;
        },
    );
}

/// ASSS `Image`.
pub fn set_image(zone: &mut Zone, target: Target, id: u16, image: u8) {
    extended(zone, target, id, object_change::IMAGE, |next, _| {
        next.image = image;
    });
}

/// ASSS `Layer`.
pub fn set_layer(zone: &mut Zone, target: Target, id: u16, layer: u8) {
    extended(zone, target, id, object_change::LAYER, |next, _| {
        next.layer = layer;
    });
}

/// ASSS `Timer`.
pub fn set_timer(zone: &mut Zone, target: Target, id: u16, time: u16) {
    extended(zone, target, id, object_change::TIME, |next, _| {
        next.set_display_time(time);
    });
}

/// ASSS `Mode`.
pub fn set_mode(zone: &mut Zone, target: Target, id: u16, mode: u8) {
    extended(zone, target, id, object_change::MODE, |next, _| {
        next.set_mode(mode);
    });
}

// ---- relaying bots' packets ----------------------------------------------

/// Records an arena-wide `0x36` a bot sent. As in ASSS a field is only kept
/// when it differs from the default, so a bot cannot record a change back to
/// the value the file declared.
fn record_changes(zone: &mut Zone, arena: ArenaId, changes: &[ObjectChange]) {
    for c in changes {
        let Some(o) = object_mut(zone, arena, c.data.id()) else {
            continue;
        };
        let d = o.defaults;
        if c.mask & object_change::POSITION != 0
            && (c.data.x_word != d.x_word || c.data.y_word != d.y_word)
        {
            o.current.x_word = c.data.x_word;
            o.current.y_word = c.data.y_word;
        }
        if c.mask & object_change::IMAGE != 0 && c.data.image != d.image {
            o.current.image = c.data.image;
        }
        if c.mask & object_change::LAYER != 0 && c.data.layer != d.layer {
            o.current.layer = c.data.layer;
        }
        if c.mask & object_change::TIME != 0 && c.data.display_time() != d.display_time() {
            o.current.set_display_time(c.data.display_time());
        }
        if c.mask & object_change::MODE != 0 && c.data.mode() != d.mode() {
            o.current.set_mode(c.data.mode());
        }
    }
}

/// ASSS `PBroadcast`: a bot relaying an object packet through C2S `0x0A`.
fn on_rebroadcast(zone: &mut Zone, from: PlayerId, pkt: &[u8]) {
    let Ok(reb) = Rebroadcast::decode(pkt) else {
        tracing::warn!(target: "objects", pid = from.0, len = pkt.len(), "bad rebroadcast packet");
        return;
    };
    let Some(player) = zone.players.get(from) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (player.arena, player.status) else {
        tracing::warn!(target: "objects", pid = from.0, "rebroadcast from a player who isn't playing");
        return;
    };
    let allow = zone
        .players
        .get(from)
        .and_then(|p| p.ext.get::<Allow>())
        .map_or(Relay::None, |a| a.0);
    if allow == Relay::None {
        tracing::warn!(target: "objects", pid = from.0, "rebroadcast without permission");
        return;
    }
    let body = reb.packet();
    // `Rebroadcast` refuses an empty packet, so there is always a type byte
    let kind = body[0];
    let to_arena = reb.target == -1;
    match kind {
        // a relayed 0x00 would look like a core packet to the client
        0 => return,
        s2c::kind::MOVE_OBJECTS => match MoveObjects::decode(body) {
            Ok(m) if to_arena => record_changes(zone, arena, &m.changes),
            Ok(_) => {}
            Err(_) => {
                tracing::info!(target: "objects", pid = from.0, "invalid move object rebroadcast");
                return;
            }
        },
        s2c::kind::TOGGLE_OBJECTS => match ToggleObjects::decode(body) {
            Ok(t) if to_arena => {
                for item in t.toggles {
                    remember_toggle(zone, arena, item.id(), item.on());
                }
            }
            Ok(_) => {}
            Err(_) => {
                tracing::info!(target: "objects", pid = from.0, "invalid toggle object rebroadcast");
                return;
            }
        },
        _ if allow != Relay::Any => {
            tracing::info!(target: "objects", pid = from.0, kind, "not authorized to rebroadcast this packet");
            return;
        }
        _ => {}
    }
    let target = reb.target;
    let body = reb.into_packet();
    if to_arena {
        zone.send_to_arena(arena, None, &body, SendFlags::RELIABLE);
        return;
    }
    let Ok(id) = u16::try_from(target) else {
        return;
    };
    let to = PlayerId(id);
    let same = zone
        .players
        .get(to)
        .is_some_and(|p| p.arena == Some(arena) && p.status == PlayerStatus::Playing);
    if same {
        zone.send(to, &body, SendFlags::RELIABLE);
    }
}

// ---- commands ------------------------------------------------------------

/// Help every `?obj*` command ends with, as in ASSS.
const OBJ_COMMANDS: &str = "Object commands: ?objon ?objoff ?objset ?objmove ?objimage ?objlayer ?objtimer ?objmode ?objinfo ?objlist\n";

const LAYER_NAMES: [&str; 8] = [
    "BelowAll",
    "AfterBackground",
    "AfterTiles",
    "AfterWeapons",
    "AfterShips",
    "AfterGauges",
    "AfterChat",
    "TopMost",
];

const MODE_NAMES: [&str; 6] = [
    "ShowAlways",
    "EnterZone",
    "EnterArena",
    "Kill",
    "Death",
    "ServerControlled",
];

const REFERENCE_NAMES: [&str; 12] = [
    "Normal - from upper left",
    "C - Screen center",
    "B - Bottom right corner",
    "S - Stats box, lower right corner",
    "G - Top right corner of specials",
    "F - Bottom right corner of specials",
    "E - Below energy bar & spec data",
    "T - Top left corner of chat",
    "R - Top left corner of radar",
    "O - Top left corner of radar's text (clock/location)",
    "W - Top left corner of weapons",
    "V - Bottom left corner of weapons",
];

fn name_of(names: &[&'static str], code: u8) -> &'static str {
    names.get(usize::from(code)).copied().unwrap_or("?")
}

fn code_of(names: &[&'static str], word: &str) -> Option<u8> {
    names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(word))
        .and_then(|i| u8::try_from(i).ok())
}

/// The letter in front of a screen coordinate, e.g. `C` in `?objmove 5 C0 C0`.
fn reference_of(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        'c' => reference::CENTER,
        'b' => reference::BOTTOM_RIGHT,
        's' => reference::STATS_BOX,
        'g' => reference::SPECIALS_TOP_RIGHT,
        'f' => reference::SPECIALS_BOTTOM_RIGHT,
        'e' => reference::BELOW_ENERGY,
        't' => reference::CHAT_TOP_LEFT,
        'r' => reference::RADAR_TOP_LEFT,
        'o' => reference::RADAR_TEXT_TOP_LEFT,
        'w' => reference::WEAPONS_TOP_LEFT,
        'v' => reference::WEAPONS_BOTTOM_LEFT,
        _ => return None,
    })
}

/// An object id: 15 bits, so the high bit of the toggle word stays ours.
fn parse_id(s: &str) -> Option<u16> {
    s.trim().parse::<u16>().ok().filter(|&id| id < 0x8000)
}

/// `[<letter>]<number>`: a coordinate with an optional reference point.
fn parse_coord(field: &str) -> Option<(u8, i16)> {
    let (r, rest) = match field.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => (reference_of(c)?, &field[c.len_utf8()..]),
        _ => (reference::NORMAL, field),
    };
    let n = rest.parse::<i32>().ok()?;
    Some((r, n.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16))
}

/// `?objset`'s argument: `[+-]<id>` repeated. Unlike ASSS's parser this
/// reads every item, not just the first.
fn parse_set(params: &str) -> Vec<(u16, bool)> {
    let mut out = Vec::new();
    let mut rest = params;
    while let Some(i) = rest.find(['+', '-']) {
        let on = rest.as_bytes()[i] == b'+';
        let after = &rest[i + 1..];
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if let Some(id) = parse_id(&after[..end]) {
            out.push((id, on));
        }
        rest = &after[end..];
    }
    out
}

fn bad_id(zone: &mut Zone, inv: &Invocation<'_>) {
    message(
        zone,
        inv.sender,
        &format!("Invalid object id. Please read help for ?{}", inv.name),
    );
}

fn c_objon(zone: &mut Zone, inv: &Invocation<'_>) {
    match parse_id(inv.params) {
        Some(id) => toggle(zone, inv.target, id, true),
        None => bad_id(zone, inv),
    }
}

fn c_objoff(zone: &mut Zone, inv: &Invocation<'_>) {
    match parse_id(inv.params) {
        Some(id) => toggle(zone, inv.target, id, false),
        None => bad_id(zone, inv),
    }
}

fn c_objset(zone: &mut Zone, inv: &Invocation<'_>) {
    let items = parse_set(inv.params);
    if items.is_empty() {
        bad_id(zone, inv);
        return;
    }
    toggle_set(zone, inv.target, &items);
}

fn c_objmove(zone: &mut Zone, inv: &Invocation<'_>) {
    let mut words = inv.params.split_whitespace();
    let parsed = (|| {
        let id = parse_id(words.next()?)?;
        let (x_ref, x) = parse_coord(words.next()?)?;
        let (y_ref, y) = parse_coord(words.next()?)?;
        Some((id, x, y, x_ref, y_ref))
    })();
    match parsed {
        Some((id, x, y, x_ref, y_ref)) => {
            move_object(zone, inv.target, id, Place { x, y, x_ref, y_ref })
        }
        None => message(
            zone,
            inv.sender,
            "Invalid syntax. Please read help for ?objmove",
        ),
    }
}

/// `?objimage` and `?objtimer`: an id and a number.
fn two_numbers(params: &str) -> Option<(u16, u32)> {
    let mut words = params.split_whitespace();
    let id = parse_id(words.next()?)?;
    let n = words.next()?.parse::<u32>().ok()?;
    Some((id, n))
}

fn c_objimage(zone: &mut Zone, inv: &Invocation<'_>) {
    match two_numbers(inv.params).and_then(|(id, n)| Some((id, u8::try_from(n).ok()?))) {
        Some((id, image)) => set_image(zone, inv.target, id, image),
        None => message(
            zone,
            inv.sender,
            "Invalid syntax. Please read help for ?objimage",
        ),
    }
}

fn c_objtimer(zone: &mut Zone, inv: &Invocation<'_>) {
    // the display time is 12 bits wide on the wire
    match two_numbers(inv.params).filter(|&(_, t)| t < 0x1000) {
        Some((id, time)) => set_timer(zone, inv.target, id, time as u16),
        None => message(
            zone,
            inv.sender,
            "Invalid syntax. Please read help for ?objtimer",
        ),
    }
}

/// `?objlayer` and `?objmode`: an id and a name from a table.
fn id_and_name(params: &str, names: &[&'static str]) -> Option<(u16, u8)> {
    let mut words = params.split_whitespace();
    let id = parse_id(words.next()?)?;
    let code = code_of(names, words.next()?)?;
    Some((id, code))
}

fn c_objlayer(zone: &mut Zone, inv: &Invocation<'_>) {
    match id_and_name(inv.params, &LAYER_NAMES) {
        Some((id, code)) => set_layer(zone, inv.target, id, code),
        None => message(
            zone,
            inv.sender,
            "Invalid syntax. Please read help for ?objlayer",
        ),
    }
}

fn c_objmode(zone: &mut Zone, inv: &Invocation<'_>) {
    match id_and_name(inv.params, &MODE_NAMES) {
        Some((id, code)) => set_mode(zone, inv.target, id, code),
        None => message(
            zone,
            inv.sender,
            "Invalid syntax. Please read help for ?objmode",
        ),
    }
}

fn c_objinfo(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(id) = parse_id(inv.params) else {
        bad_id(zone, inv);
        return;
    };
    let Some(arena) = inv.arena else { return };
    let Some(o) = object(zone, arena, id).copied() else {
        message(
            zone,
            inv.sender,
            &format!("Object {id} does not exist in any of the loaded LVZ files."),
        );
        return;
    };
    let head = format!(
        "lvz: Id:{} Off:{} Image:{} Layer:{} Mode:{}",
        id,
        u8::from(o.off),
        o.current.image,
        name_of(&LAYER_NAMES, o.current.layer),
        name_of(&MODE_NAMES, o.current.mode()),
    );
    message(zone, inv.sender, &head);
    let detail = if o.current.is_map_object() {
        format!(
            "lvz: map object coords ({}, {})",
            o.current.map_x(),
            o.current.map_y()
        )
    } else {
        format!(
            "lvz: screen object coords ({}, {}). X-offset: {}. Y-offset: {}",
            o.current.screen_x(),
            o.current.screen_y(),
            name_of(&REFERENCE_NAMES, o.current.x_ref()),
            name_of(&REFERENCE_NAMES, o.current.y_ref()),
        )
    };
    message(zone, inv.sender, &detail);
}

/// Joins ids into lines of at most 80 characters (ASSS `SendWrappedText`).
fn wrap_ids(ids: &[u16]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let s = id.to_string();
        match out.last_mut() {
            Some(l) if l.len() + s.len() + 2 <= 80 => {
                l.push_str(", ");
                l.push_str(&s);
            }
            _ => out.push(s),
        }
    }
    out
}

fn c_objlist(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let ids: Vec<u16> = objects_of(zone, arena)
        .map(|objs| {
            objs.list
                .iter()
                .filter(|o| o.current.mode() == mode::SERVER_CONTROLLED)
                .map(|o| o.defaults.id())
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        message(zone, inv.sender, "0 ServerControlled objects.");
        return;
    }
    let plural = if ids.len() == 1 { "" } else { "s" };
    message(
        zone,
        inv.sender,
        &format!("{} ServerControlled object{plural}:", ids.len()),
    );
    for line in wrap_ids(&ids) {
        message(zone, inv.sender, &line);
    }
}

// ---- the module ----------------------------------------------------------

/// The `objects` module.
pub struct ObjectsModule;

impl Module for ObjectsModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Objects>(me, Scope::Global, Arc::new(ObjectsImpl));
        zone.on_packet(me, c2s::kind::REBROADCAST, on_rebroadcast);
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create => load_objects(z, ev.arena),
            ArenaActionKind::Destroy => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.remove::<ArenaObjects>();
                }
            }
            _ => {}
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action != Action::EnterArena {
                return;
            }
            // ASSS decides this on PA_ENTERARENA and sends the state on
            // PA_ENTERGAME; here the arena response has just gone out, which
            // is the same moment the client is ready for both.
            let caps = z.iface::<dyn Capabilities>();
            let allow = match caps {
                Some(c) if c.has_capability(z, ev.player, CAP_BROADCAST_ANY) => Relay::Any,
                Some(c) if c.has_capability(z, ev.player, CAP_BROADCAST_BOT) => Relay::Objects,
                _ => Relay::None,
            };
            if let Some(p) = z.players.get_mut(ev.player) {
                *p.ext.get_or_default::<Allow>() = Allow(allow);
            }
            send_state(z, ev.player);
        });
        let help = |what: &str, args: &str, body: &str| {
            format!("Module: objects\nTargets: {what}\nArgs: {args}\n{body}\n{OBJ_COMMANDS}")
        };
        zone.add_command(
            me,
            "objon",
            None,
            Some(&help(
                "any",
                "<object id>",
                "Toggles the specified object on.",
            )),
            c_objon,
        );
        zone.add_command(
            me,
            "objoff",
            None,
            Some(&help(
                "any",
                "<object id>",
                "Toggles the specified object off.",
            )),
            c_objoff,
        );
        zone.add_command(
            me,
            "objset",
            None,
            Some(&help(
                "any",
                "[+/-]<object id> [+/-]<id> ...",
                "Toggles the specified objects on and off.",
            )),
            c_objset,
        );
        zone.add_command(
            me,
            "objmove",
            None,
            Some(&help(
                "any",
                "<id> <x> <y> (map object) or <id> [CBSGFETROWV]<x> [CBSGFETROWV]<y> (screen object)",
                "Moves an LVZ map or screen object. Coordinates are in pixels.",
            )),
            c_objmove,
        );
        zone.add_command(
            me,
            "objimage",
            None,
            Some(&help(
                "any",
                "<id> <image>",
                "Changes the image associated with an object id.",
            )),
            c_objimage,
        );
        zone.add_command(
            me,
            "objlayer",
            None,
            Some(&help(
                "any",
                "<id> <layer code>",
                "Changes the layer an object is drawn on. Layer codes:\nBelowAll  AfterBackground  AfterTiles  AfterWeapons\nAfterShips  AfterGauges  AfterChat  TopMost",
            )),
            c_objlayer,
        );
        zone.add_command(
            me,
            "objtimer",
            None,
            Some(&help(
                "any",
                "<id> <timer>",
                "Changes how long an object shows for, in tenths of a second (0 = until it is turned off).",
            )),
            c_objtimer,
        );
        zone.add_command(
            me,
            "objmode",
            None,
            Some(&help(
                "any",
                "<id> <mode code>",
                "Changes the mode associated with an object id. Mode codes:\nShowAlways  EnterZone  EnterArena  Kill  Death  ServerControlled",
            )),
            c_objmode,
        );
        zone.add_command(
            me,
            "objinfo",
            None,
            Some(&help(
                "none",
                "<id>",
                "Reports all known information about the object.",
            )),
            c_objinfo,
        );
        zone.add_command(
            me,
            "objlist",
            None,
            Some(&help(
                "none",
                "none",
                "Lists all ServerControlled object ids. Use ?objinfo <id> for attributes.",
            )),
            c_objlist,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use oss_proto::objects::layer;

    use super::*;

    #[test]
    fn objset_reads_every_item() {
        // ASSS's own parser gets the second item wrong; this is the bug the
        // module header records as divergence 1.
        assert_eq!(
            parse_set("+1 -2 +3"),
            vec![(1, true), (2, false), (3, true)]
        );
        assert_eq!(parse_set("+1-2"), vec![(1, true), (2, false)]);
        assert_eq!(parse_set(""), vec![]);
        assert_eq!(parse_set("5"), vec![]);
    }

    #[test]
    fn coordinates_take_a_reference_letter() {
        assert_eq!(parse_coord("100"), Some((reference::NORMAL, 100)));
        assert_eq!(parse_coord("C-50"), Some((reference::CENTER, -50)));
        assert_eq!(parse_coord("v7"), Some((reference::WEAPONS_BOTTOM_LEFT, 7)));
        assert_eq!(parse_coord("x7"), None);
        assert_eq!(parse_coord(""), None);
        // out of range coordinates clamp rather than wrapping round
        assert_eq!(parse_coord("99999"), Some((reference::NORMAL, i16::MAX)));
    }

    #[test]
    fn layer_and_mode_names_round_trip() {
        assert_eq!(code_of(&LAYER_NAMES, "topmost"), Some(layer::TOP_MOST));
        assert_eq!(
            code_of(&MODE_NAMES, "ServerControlled"),
            Some(mode::SERVER_CONTROLLED)
        );
        assert_eq!(code_of(&MODE_NAMES, "nonsense"), None);
        assert_eq!(name_of(&LAYER_NAMES, layer::AFTER_SHIPS), "AfterShips");
        assert_eq!(name_of(&LAYER_NAMES, 200), "?");
    }

    #[test]
    fn ids_are_fifteen_bits() {
        assert_eq!(parse_id("0"), Some(0));
        assert_eq!(parse_id("32767"), Some(32767));
        assert_eq!(parse_id("32768"), None);
        assert_eq!(parse_id("-1"), None);
        assert_eq!(parse_id("x"), None);
    }
}
