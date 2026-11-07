//! `chat`: routes chat messages and sends server messages (ASSS `chat.c`).
//!
//! Handles public, macro, team, enemy-team, private, remote-private and staff
//! (`\` prefix) chat, and hands `?`/`*` lines to [`super::cmdman`]
//! (several commands per line with `?|cmd1|cmd2`, up to
//! `Chat:CommandLimit`). Control characters are replaced with `_`, sounds
//! need the `sendsoundmessages` capability. Not yet ported: chat masks
//! (`?shutup`), flood control, the obscenity filter and chat channels.

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
    ChatImpl.send_any_message(zone, to, kind, sound, from, text);
}

fn on_chat(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
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
            }
        }
        chat_type::ARENA | chat_type::SERVER_ERROR | chat_type::WARNING => {
            tracing::warn!(target: "chat", pid = p.0, kind = body.kind, "client sent a server-only chat type");
        }
        chat_type::CHANNEL => {
            tracing::debug!(target: "chat", pid = p.0, "chat channels not implemented");
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
        Ok(())
    }
}
