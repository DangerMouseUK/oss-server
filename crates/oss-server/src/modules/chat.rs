//! `chat`: routes chat messages and sends server messages (ASSS `chat.c`).
//!
//! Handles public, macro, team, enemy-team, private, remote-private and staff
//! (`\` prefix) chat, and hands `?`/`*` lines to [`super::cmdman`]
//! (several commands per line with `?|cmd1|cmd2`, up to
//! `Chat:CommandLimit`). Control characters are replaced with `_`, sounds
//! need the `sendsoundmessages` capability.
//!
//! Also here: per-player chat masks ([`set_player_chat_mask`], with a
//! timeout) and the arena's own `Chat:RestrictChat` mask; flood control
//! (`Chat:FloodLimit`, which shuts a player up for `Chat:FloodShutup`
//! seconds). The obscenity filter is [`super::obscene`]. Chat channels,
//! remote private messages and squad messages are routed by
//! [`super::billing_ssc`] and need a connected biller, as in ASSS.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::common::chat_type;
use oss_proto::s2c::ChatBody;
use oss_proto::s2c::S2CChat;

use crate::arena::ArenaId;
use crate::commands::Target;
use crate::config::{latin1, to_latin1};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::capman::{CAP_MOD_CHAT, CAP_SEND_MOD_CHAT, CAP_SOUND_MESSAGES, Capabilities};

/// Longest message text sent (ASSS: `MAXPACKET - 10`).
pub const MAX_TEXT: usize = 502;

/// Sending chat to players (ASSS `Ichat`).
pub trait Chat: Send + Sync {
    /// Arena-style (green) message to one player.
    fn send_message(&self, zone: &mut Zone, to: PlayerId, text: &str);
    /// Arena-style message to several players.
    fn send_set_message(&self, zone: &mut Zone, to: &[PlayerId], text: &str);
    /// Arena-style message to everyone in an arena.
    fn send_arena_message(&self, zone: &mut Zone, arena: ArenaId, text: &str);
    /// Any message type, from any player (`from` = -1 for none).
    fn send_any_message(
        &self,
        zone: &mut Zone,
        to: &[PlayerId],
        kind: u8,
        sound: u8,
        from: i16,
        text: &str,
    );
}

/// ASSS `CB_CHATMSG`: a player said something (after routing).
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Sender.
    pub from: PlayerId,
    /// Arena of the sender.
    pub arena: Option<ArenaId>,
    /// [`chat_type`].
    pub kind: u8,
    /// Sound.
    pub sound: u8,
    /// Private target.
    pub to: Option<PlayerId>,
    /// Team target.
    pub freq: Option<u16>,
    /// Text.
    pub text: String,
}

impl Event for ChatMessage {
    fn arena(&self) -> Option<ArenaId> {
        self.arena
    }
}

fn reliable(zone: &Zone) -> SendFlags {
    if zone.cfg.global().get_int("Chat", "MessageReliable", 1) != 0 {
        SendFlags::RELIABLE
    } else {
        SendFlags::UNRELIABLE
    }
}

fn encode(kind: u8, sound: u8, from: i16, text: &str) -> Option<Vec<u8>> {
    let mut t = to_latin1(text);
    if t.len() > MAX_TEXT {
        t.truncate(MAX_TEXT);
    }
    let mut buf = Vec::with_capacity(t.len() + 6);
    S2CChat(ChatBody {
        kind,
        sound,
        pid: from,
        text: t,
    })
    .encode(&mut buf);
    Some(buf)
}

/// The chat implementation.
pub struct ChatImpl;

impl Chat for ChatImpl {
    fn send_message(&self, zone: &mut Zone, to: PlayerId, text: &str) {
        self.send_any_message(zone, &[to], chat_type::ARENA, 0, -1, text);
    }

    fn send_set_message(&self, zone: &mut Zone, to: &[PlayerId], text: &str) {
        self.send_any_message(zone, to, chat_type::ARENA, 0, -1, text);
    }

    fn send_arena_message(&self, zone: &mut Zone, arena: ArenaId, text: &str) {
        let to: Vec<PlayerId> = zone.players.in_arena(arena).map(|p| p.id).collect();
        self.send_any_message(zone, &to, chat_type::ARENA, 0, -1, text);
    }

    fn send_any_message(
        &self,
        zone: &mut Zone,
        to: &[PlayerId],
        kind: u8,
        sound: u8,
        from: i16,
        text: &str,
    ) {
        if let Some(buf) = encode(kind, sound, from, text) {
            let flags = reliable(zone);
            zone.send_many(to.iter().copied(), &buf, flags);
        }
    }
}

/// Convenience: an arena-style message to one player, if chat is loaded.
pub fn message(zone: &mut Zone, to: PlayerId, text: &str) {
    if let Some(c) = zone.iface::<dyn Chat>() {
        c.send_message(zone, to, text);
    }
}

/// Sends a remote private message from another zone (ASSS
/// `SendRemotePrivMessage`): `(sender)>msg`, or `(#squad)(sender)>msg` for a
/// squad message.
pub fn send_remote_priv(
    zone: &mut Zone,
    to: &[PlayerId],
    sound: u8,
    squad: Option<&str>,
    sender: &str,
    msg: &str,
) {
    let text = match squad {
        Some(sq) => format!("(#{sq})({sender})>{msg}"),
        None => format!("({sender})>{msg}"),
    };
    if let Some(c) = zone.iface::<dyn Chat>() {
        c.send_any_message(zone, to, chat_type::REMOTE_PRIVATE, sound, -1, &text);
    }
}

/// Splits `text` into lines of at most `width` bytes at spaces (ASSS
/// `wrap_text`): words longer than `width - 1` are cut short.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.clamp(2, 250);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut rest = Some(text);
    while let Some(r) = rest.filter(|r| !r.is_empty()) {
        let (word, next) = match r.split_once(' ') {
            Some((w, n)) => (w, Some(n)),
            None => (r, None),
        };
        let mut end = word.len().min(width - 1);
        while !word.is_char_boundary(end) {
            end -= 1;
        }
        let word = &word[..end];
        if line.len() + word.len() > width {
            if line.ends_with(' ') {
                line.pop();
            }
            out.push(std::mem::take(&mut line));
        }
        line.push_str(word);
        if next.is_some_and(|n| !n.is_empty()) {
            line.push(' ');
        }
        rest = next;
    }
    if !line.is_empty() && !line.starts_with(' ') {
        out.push(line);
    }
    out
}

/// Sends `text` wrapped at 80 columns (ASSS `SendWrappedText`).
pub fn wrapped_message(zone: &mut Zone, to: PlayerId, text: &str) {
    for l in wrap_text(text, 80) {
        message(zone, to, &l);
    }
}

fn is_command(text: &str) -> bool {
    let mut c = text.chars();
    matches!(c.next(), Some('?' | '*')) && c.next().is_some()
}

fn has_cap(zone: &mut Zone, p: PlayerId, cap: &str) -> bool {
    zone.iface::<dyn Capabilities>()
        .is_some_and(|c| c.has_capability(zone, p, cap))
}

fn run_commands(zone: &mut Zone, p: PlayerId, text: &str, target: Target) {
    let limit = usize::try_from(zone.cfg.global().get_int("Chat", "CommandLimit", 5)).unwrap_or(5);
    let text = text.strip_prefix(['?', '*']).unwrap_or(text);
    if let Some(multi) = text.strip_prefix('|') {
        for cmd in multi.split('|').filter(|s| !s.is_empty()).take(limit) {
            super::cmdman::command(zone, p, cmd, target);
        }
    } else if limit > 0 {
        super::cmdman::command(zone, p, text, target);
    }
}

fn reply(zone: &mut Zone, to: &[PlayerId], kind: u8, sound: u8, from: i16, text: &str) {
    // players with the obscenity filter on get a filtered copy (or none,
    // with Chat:FilterMode = 0), as in ASSS send_reply
    let Some(ob) = zone.iface::<dyn super::obscene::Obscene>() else {
        ChatImpl.send_any_message(zone, to, kind, sound, from, text);
        return;
    };
    let (filtered, plain): (Vec<PlayerId>, Vec<PlayerId>) = to
        .iter()
        .partition(|p| zone.players.get(**p).is_some_and(|x| x.obscenity_filter));
    ChatImpl.send_any_message(zone, &plain, kind, sound, from, text);
    if filtered.is_empty() {
        return;
    }
    let mut clean = text.to_owned();
    let hit = ob.filter(zone, &mut clean);
    if !hit || zone.cfg.global().get_int("Chat", "FilterMode", 1) != 0 {
        ChatImpl.send_any_message(zone, &filtered, kind, sound, from, &clean);
    }
}

fn route_chat(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if pkt.len() < 6 || pkt.len() > 500 {
        tracing::warn!(target: "chat", pid = p.0, len = pkt.len(), "bad chat packet length");
        return;
    }
    let Ok(c2s::C2SChat(body)) = c2s::C2SChat::decode(pkt) else {
        return;
    };
    let Some(pl) = zone.players.get(p) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (pl.arena, pl.status) else {
        return;
    };
    let my_freq = pl.freq;
    let name = pl.name.clone();
    let text: String = latin1(&body.text)
        .chars()
        .map(|c| {
            if (c as u32) < 32 || c as u32 == 127 || c as u32 == 255 {
                '_'
            } else {
                c
            }
        })
        .collect();
    let sound = if has_cap(zone, p, CAP_SOUND_MESSAGES) {
        body.sound
    } else {
        0
    };
    let pid = p.0 as i16;

    match body.kind {
        chat_type::PUBLIC | chat_type::PUBLIC_MACRO => {
            if let Some(msg) = text.strip_prefix('\\') {
                if has_cap(zone, p, CAP_SEND_MOD_CHAT) {
                    let staff: Vec<PlayerId> = zone.players.ids();
                    let mut to = Vec::new();
                    for s in staff {
                        if s != p && has_cap(zone, s, CAP_MOD_CHAT) {
                            to.push(s);
                        }
                    }
                    reply(
                        zone,
                        &to,
                        chat_type::SERVER_ERROR,
                        sound,
                        -1,
                        &format!("{name}> {msg}"),
                    );
                    fire(
                        zone,
                        p,
                        Some(arena),
                        chat_type::SERVER_ERROR,
                        sound,
                        None,
                        None,
                        msg,
                    );
                } else {
                    message(
                        zone,
                        p,
                        "You aren't allowed to use the staff chat. If you need to send a message to the zone staff, use ?cheater.",
                    );
                }
            } else if is_command(&text) {
                run_commands(zone, p, &text, Target::Arena(arena));
            } else {
                let to: Vec<PlayerId> = zone
                    .players
                    .in_arena(arena)
                    .map(|o| o.id)
                    .filter(|&o| o != p)
                    .collect();
                reply(zone, &to, body.kind, sound, pid, &text);
                fire(zone, p, Some(arena), body.kind, sound, None, None, &text);
            }
        }
        chat_type::FREQ | chat_type::ENEMY_FREQ => {
            let freq = if body.kind == chat_type::FREQ {
                my_freq
            } else {
                match zone.players.get(PlayerId(body.pid as u16)) {
                    Some(t) if t.arena == Some(arena) => t.freq,
                    Some(_) => {
                        tracing::warn!(target: "chat", pid = p.0, "cross-arena enemy freq message");
                        return;
                    }
                    None => return,
                }
            };
            if is_command(&text) {
                run_commands(zone, p, &text, Target::Freq(arena, freq));
            } else {
                let kind = if freq == my_freq {
                    chat_type::FREQ
                } else {
                    chat_type::ENEMY_FREQ
                };
                let to: Vec<PlayerId> = zone
                    .players
                    .iter()
                    .filter(|o| o.freq == freq && o.arena == Some(arena) && o.id != p)
                    .map(|o| o.id)
                    .collect();
                reply(zone, &to, kind, sound, pid, &text);
                fire(
                    zone,
                    p,
                    Some(arena),
                    chat_type::FREQ,
                    sound,
                    None,
                    Some(freq),
                    &text,
                );
            }
        }
        chat_type::PRIVATE => {
            let t = PlayerId(body.pid as u16);
            match zone.players.get(t) {
                Some(tp) if tp.arena == Some(arena) => {}
                Some(_) => {
                    tracing::warn!(target: "chat", pid = p.0, "cross-arena private message");
                    return;
                }
                None => return,
            }
            if is_command(&text) {
                run_commands(zone, p, &text, Target::Player(t));
            } else {
                reply(zone, &[t], chat_type::PRIVATE, sound, pid, &text);
                fire(
                    zone,
                    p,
                    Some(arena),
                    chat_type::PRIVATE,
                    sound,
                    Some(t),
                    None,
                    &text,
                );
            }
        }
        chat_type::REMOTE_PRIVATE => {
            let Some(rest) = text.strip_prefix(':') else {
                tracing::warn!(target: "chat", pid = p.0, "malformed remote private message");
                return;
            };
            let Some((dest, msg)) = rest.split_once(':') else {
                tracing::warn!(target: "chat", pid = p.0, "malformed remote private message");
                return;
            };
            let dest: String = dest.chars().take(20).collect();
            let d = zone
                .players
                .find_by_name(&dest)
                .filter(|d| d.status == PlayerStatus::Playing)
                .map(|d| d.id);
            if is_command(msg) {
                if let Some(d) = d {
                    run_commands(zone, p, msg, Target::Player(d));
                }
            } else if let Some(d) = d {
                reply(
                    zone,
                    &[d],
                    chat_type::REMOTE_PRIVATE,
                    sound,
                    -1,
                    &format!("({name})>{msg}"),
                );
                fire(
                    zone,
                    p,
                    Some(arena),
                    chat_type::REMOTE_PRIVATE,
                    sound,
                    Some(d),
                    None,
                    msg,
                );
            } else if zone.players.find_by_name(&dest).is_none() {
                // not here: the biller (if any) delivers it. As in ASSS the
                // text is the whole ":target:message".
                fire(
                    zone,
                    p,
                    Some(arena),
                    chat_type::REMOTE_PRIVATE,
                    sound,
                    None,
                    None,
                    &text,
                );
            }
        }
        chat_type::ARENA | chat_type::SERVER_ERROR | chat_type::WARNING => {
            tracing::warn!(target: "chat", pid = p.0, kind = body.kind, "client sent a server-only chat type");
        }
        chat_type::CHANNEL => {
            // "text" or "channel;text": delivered by the biller
            fire(
                zone,
                p,
                Some(arena),
                chat_type::CHANNEL,
                sound,
                None,
                None,
                &text,
            );
        }
        k => tracing::warn!(target: "chat", pid = p.0, kind = k, "undefined chat type"),
    }
}

#[allow(clippy::too_many_arguments)]
fn fire(
    zone: &mut Zone,
    from: PlayerId,
    arena: Option<ArenaId>,
    kind: u8,
    sound: u8,
    to: Option<PlayerId>,
    freq: Option<u16>,
    text: &str,
) {
    tracing::debug!(target: "chat", pid = from.0, kind, "{text}");
    zone.fire(&ChatMessage {
        from,
        arena,
        kind,
        sound,
        to,
        freq,
        text: text.to_owned(),
    });
}

/// The `chat` module.
pub struct ChatModule;

impl Module for ChatModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Chat>(me, Scope::Global, Arc::new(ChatImpl));
        zone.on_packet(me, c2s::kind::CHAT, on_chat);
        zone.on::<super::arenaman::ArenaAction>(me, Scope::Global, |z, ev| {
            use super::arenaman::ArenaActionKind;
            if matches!(
                ev.action,
                ArenaActionKind::Create | ArenaActionKind::ConfChanged
            ) {
                load_arena_mask(z, ev.arena);
            }
        });
        Ok(())
    }
}

/// ASSS message types used in chat masks (`MSG_*`).
pub mod mask_type {
    /// Public macros.
    pub const PUB_MACRO: u8 = 1;
    /// Public messages.
    pub const PUB: u8 = 2;
    /// Team messages.
    pub const FREQ: u8 = 3;
    /// Messages to another team.
    pub const NMEFREQ: u8 = 4;
    /// Private messages.
    pub const PRIV: u8 = 5;
    /// Remote private messages.
    pub const REMOTE_PRIV: u8 = 7;
    /// Chat channel messages.
    pub const CHAT: u8 = 9;
    /// Staff chat.
    pub const MODCHAT: u8 = 10;
    /// Commands.
    pub const COMMAND: u8 = 11;
    /// Commands for the biller.
    pub const BCOMMAND: u8 = 12;
}

/// May send everything flood control would stop (ASSS `CAP_CANSPAM`).
pub const CAP_CAN_SPAM: &str = "unlimitedchat";

#[derive(Default)]
struct PlayerMask {
    mask: u16,
    /// unix time the mask expires; 0 = session-long
    expires: u64,
    msgs: i32,
    last_check: u64,
}

/// An arena's chat mask (`Chat:RestrictChat`).
#[derive(Default)]
struct ArenaMask(u16);

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn expire_mask(zone: &mut Zone, p: PlayerId) -> u16 {
    let now_ticks = zone.now().0;
    let Some(pm) = zone
        .players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<PlayerMask>())
    else {
        return 0;
    };
    if pm.expires > 0 && unix_now() > pm.expires {
        pm.mask = 0;
        pm.expires = 0;
    }
    let d = now_ticks.saturating_sub(pm.last_check) / 100;
    pm.msgs = if d >= 31 { 0 } else { pm.msgs >> d };
    pm.last_check = now_ticks;
    pm.mask
}

/// ASSS `ok()`: may `p` send a message of `kind` (a [`mask_type`])?
pub fn allowed(zone: &mut Zone, p: PlayerId, kind: u8) -> bool {
    let pmask = expire_mask(zone, p);
    let amask = zone
        .players
        .get(p)
        .and_then(|x| x.arena)
        .and_then(|a| zone.arenas.get(a))
        .and_then(|a| a.ext.get::<ArenaMask>())
        .map_or(0, |m| m.0);
    (pmask | amask) & (1 << kind) == 0
}

/// ASSS `SetPlayerChatMask`: `timeout` in seconds, 0 for the session.
pub fn set_player_chat_mask(zone: &mut Zone, p: PlayerId, mask: u16, timeout: u64) {
    if let Some(pm) = zone
        .players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<PlayerMask>())
    {
        pm.mask = mask;
        pm.expires = if timeout == 0 {
            0
        } else {
            unix_now() + timeout
        };
    }
}

/// A player's current chat mask.
pub fn player_chat_mask(zone: &mut Zone, p: PlayerId) -> u16 {
    expire_mask(zone, p)
}

/// Sets an arena's chat mask (until its config changes).
pub fn set_arena_chat_mask(zone: &mut Zone, a: crate::arena::ArenaId, mask: u16) {
    if let Some(x) = zone.arenas.get_mut(a) {
        x.ext.insert(ArenaMask(mask));
    }
}

/// ASSS `check_flood`.
fn check_flood(zone: &mut Zone, p: PlayerId) {
    let limit = zone.cfg.global().get_int("Chat", "FloodLimit", 10);
    let shutup = u64::try_from(zone.cfg.global().get_int("Chat", "FloodShutup", 60)).unwrap_or(60);
    let over = zone
        .players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<PlayerMask>())
        .is_some_and(|pm| {
            pm.msgs += 1;
            limit > 0 && pm.msgs >= limit
        });
    if !over || has_cap(zone, p, CAP_CAN_SPAM) {
        return;
    }
    if let Some(pm) = zone
        .players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<PlayerMask>())
    {
        pm.msgs >>= 1;
        if pm.expires != 0 {
            pm.expires += shutup;
        } else if pm.mask == 0 {
            pm.expires = unix_now() + shutup;
        }
        use mask_type::*;
        pm.mask |= 1 << PUB_MACRO
            | 1 << PUB
            | 1 << FREQ
            | 1 << NMEFREQ
            | 1 << PRIV
            | 1 << REMOTE_PRIV
            | 1 << CHAT
            | 1 << MODCHAT
            | 1 << BCOMMAND;
    }
    message(
        zone,
        p,
        &format!("You have been shut up for {shutup} seconds for flooding."),
    );
    tracing::info!(target: "chat", pid = p.0, "flooded chat, shut up for {shutup} seconds");
}

/// Which mask type a chat packet is, for the mask check.
fn classify(body: &oss_proto::s2c::ChatBody) -> u8 {
    let text = &body.text;
    let cmd = |t: &[u8]| matches!(t.first(), Some(b'?' | b'*')) && t.len() > 1;
    match body.kind {
        chat_type::PUBLIC | chat_type::PUBLIC_MACRO if text.first() == Some(&b'\\') => {
            mask_type::MODCHAT
        }
        chat_type::PUBLIC
        | chat_type::PUBLIC_MACRO
        | chat_type::FREQ
        | chat_type::ENEMY_FREQ
        | chat_type::PRIVATE
            if cmd(text) =>
        {
            mask_type::COMMAND
        }
        chat_type::PUBLIC_MACRO => mask_type::PUB_MACRO,
        chat_type::PUBLIC => mask_type::PUB,
        chat_type::FREQ => mask_type::FREQ,
        chat_type::ENEMY_FREQ => mask_type::NMEFREQ,
        chat_type::PRIVATE => mask_type::PRIV,
        chat_type::REMOTE_PRIVATE => {
            let rest = text.splitn(3, |&b| b == b':').nth(2).unwrap_or(&[]);
            if cmd(rest) {
                mask_type::COMMAND
            } else {
                mask_type::REMOTE_PRIV
            }
        }
        chat_type::CHANNEL => mask_type::CHAT,
        _ => 0,
    }
}

fn on_chat(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if let Ok(c2s::C2SChat(body)) = c2s::C2SChat::decode(pkt)
        && zone
            .players
            .get(p)
            .is_some_and(|x| x.status == PlayerStatus::Playing && x.arena.is_some())
    {
        let kind = classify(&body);
        if kind == 0 || allowed(zone, p, kind) {
            route_chat(zone, p, pkt);
        } else if kind == mask_type::MODCHAT {
            message(
                zone,
                p,
                "You aren't allowed to use the staff chat. If you need to send a message to the zone staff, use ?cheater.",
            );
        }
        check_flood(zone, p);
    } else {
        route_chat(zone, p, pkt);
    }
}

fn load_arena_mask(zone: &mut Zone, a: crate::arena::ArenaId) {
    let m = zone
        .arenas
        .get(a)
        .map_or(0, |x| x.cfg.get_int("Chat", "RestrictChat", 0));
    set_arena_chat_mask(zone, a, m as u16);
}
