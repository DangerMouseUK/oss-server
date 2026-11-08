//! `chatnet`: the text "chat protocol" over TCP (ASSS `chatnet.c` and the
//! chatnet handlers spread over its core modules), used by chat clients and
//! bots that only talk.
//!
//! Lines are `TYPE:rest`, ending in a newline. From the client:
//! - `LOGIN:version[;client name]:name:password`
//! - `GO:arena` (empty for any public arena), `LEAVE`
//! - `SEND:PUB:text`, `SEND:PUBM:text`, `SEND:CMD:command`,
//!   `SEND:PRIV:name:text`, `SEND:PRIVCMD:name:command`,
//!   `SEND:FREQ:freq:text`, `SEND:CHAT:text`, `SEND:MOD:text`
//! - `CHANGEFREQ:freq`, `NOOP`
//!
//! To the client: `LOGINOK:name`, `LOGINBAD:reason`, `INARENA:arena:freq`,
//! `PLAYER:name:ship:freq` (already there), `ENTERING:name:ship:freq`,
//! `LEAVING:name`, `SHIPFREQCHANGE:name:ship:freq`,
//! `KILL:killer:killed:bounty:flags`, `MSG:type:[name:]text` (types `ARENA`,
//! `PUB`, `PUBM`, `FREQ`, `PRIV`, `SQUAD`, `CHAT`, `MOD`, `SYSOP`) and
//! `NOOP` after three idle minutes.
//!
//! Chat clients are ordinary players of kind [`ClientKind::Chat`], always in
//! spectator mode. Their incoming lines become the equivalent game packets;
//! what the zone sends them is diverted from the game transport
//! ([`Zone::set_text_player`]) and translated back to lines here, so other
//! modules need no chat-protocol code. One incoming line is handled every
//! `Net:ChatMessageDelay` ticks (default 20). It listens on `Net:ChatListen`
//! (`port` or `ip:port`; default: the game port) once the server starts.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use oss_proto::Packet;
use oss_proto::c2s::{self, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody};
use oss_proto::wire::FixedStr;

use crate::config::{latin1, to_latin1};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus, Ship};
use crate::registry::{Owner, Scope};
use crate::time::Ticks;
use crate::zone::{Zone, ZoneStarted};

/// Longest line accepted from a client (ASSS `MAXMSGSIZE`).
pub const MAX_LINE: usize = 2035;
/// Unsent output a client may have before it is dropped.
const MAX_PENDING_OUT: usize = 256 * 1024;
/// Idle ticks (both ways) before a `NOOP` is sent.
const NOOP_AFTER: u64 = 18000;

/// Queues a text line for a chat-protocol player, in order with the
/// translated game packets. Does nothing useful for other players.
pub fn send_line(zone: &mut Zone, p: PlayerId, line: &str) {
    let mut b = Vec::with_capacity(line.len() + 1);
    // no game packet starts with 0: it marks a ready-made line
    b.push(0);
    b.extend_from_slice(&to_latin1(line));
    zone.send(p, &b, SendFlags::RELIABLE);
}

struct Conn {
    stream: TcpStream,
    inbuf: Vec<u8>,
    lines: VecDeque<Vec<u8>>,
    out: Vec<u8>,
    last_proc: Ticks,
    last_send: Ticks,
    last_recv: Ticks,
    /// names of players this client heard about, for LEAVING and the like
    names: HashMap<PlayerId, String>,
    closing: bool,
}

#[derive(Default)]
struct ChatNet {
    listener: Option<TcpListener>,
    conns: HashMap<PlayerId, Conn>,
    delay: u64,
}

/// The address chatnet listens on, once the server has started.
pub fn local_addr(zone: &Zone) -> Option<SocketAddr> {
    zone.res
        .get::<ChatNet>()
        .and_then(|c| c.listener.as_ref())
        .and_then(|l| l.local_addr().ok())
}

fn listen_addr(zone: &Zone) -> Option<SocketAddr> {
    let g = zone.cfg.global();
    // the game socket's port (as ASSS uses its first listen port)
    let game_port = crate::net::Listeners::of(zone)
        .first()
        .map_or(5000, |l| l.addr.port());
    match g.get("Net", "ChatListen").map(str::trim) {
        None | Some("") => Some(SocketAddr::from(([0, 0, 0, 0], game_port))),
        Some(spec) => match spec.split_once(':') {
            Some((ip, port)) => Some(SocketAddr::new(ip.parse().ok()?, port.trim().parse().ok()?)),
            None => Some(SocketAddr::from(([0, 0, 0, 0], spec.parse().ok()?))),
        },
    }
}

fn start(zone: &mut Zone) {
    let Some(addr) = listen_addr(zone) else {
        tracing::error!(target: "chatnet", "bad Net:ChatListen; chat protocol disabled");
        return;
    };
    match TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
        Ok(l) => {
            tracing::info!(target: "chatnet", "listening for chat clients on {}", l.local_addr().unwrap_or(addr));
            zone.res.get_or_default::<ChatNet>().listener = Some(l);
        }
        Err(e) => tracing::error!(target: "chatnet", "can't listen on {addr}: {e}"),
    }
}

fn accept(zone: &mut Zone) {
    loop {
        let Some(l) = zone.res.get::<ChatNet>().and_then(|c| c.listener.as_ref()) else {
            return;
        };
        let (stream, addr) = match l.accept() {
            Ok(x) => x,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(e) => {
                tracing::warn!(target: "chatnet", "accept failed: {e}");
                return;
            }
        };
        if stream.set_nonblocking(true).is_err() {
            continue;
        }
        let _ = stream.set_nodelay(true);
        let Some(pid) = super::core::new_connection(zone, ClientKind::Chat, Some(addr)) else {
            tracing::warn!(target: "chatnet", %addr, "player table full");
            continue;
        };
        zone.set_text_player(pid, true);
        if let Some(p) = zone.players.get_mut(pid) {
            p.client_name = "<unknown chat client>".to_owned();
        }
        let now = zone.now();
        tracing::debug!(target: "chatnet", pid = pid.0, %addr, "new connection");
        zone.res.get_or_default::<ChatNet>().conns.insert(
            pid,
            Conn {
                stream,
                inbuf: Vec::new(),
                lines: VecDeque::new(),
                out: Vec::new(),
                last_proc: Ticks(0),
                last_send: now,
                last_recv: now,
                names: HashMap::new(),
                closing: false,
            },
        );
    }
}

/// Reads what arrived; false if the connection is gone.
fn read(c: &mut Conn, now: Ticks) -> bool {
    let mut buf = [0u8; 4096];
    loop {
        if c.inbuf.len() >= MAX_LINE {
            // a full buffer without a newline: the line is too long
            if !c.inbuf.iter().any(|&b| b == b'\n' || b == b'\r') {
                c.inbuf.clear();
            }
            return true;
        }
        let room = (MAX_LINE - c.inbuf.len()).min(buf.len());
        match c.stream.read(&mut buf[..room]) {
            Ok(0) => return false,
            Ok(n) => {
                c.inbuf.extend_from_slice(&buf[..n]);
                c.last_recv = now;
                while let Some(i) = c.inbuf.iter().position(|&b| b == b'\n' || b == b'\r') {
                    let line: Vec<u8> = c.inbuf.drain(..i).collect();
                    while matches!(c.inbuf.first(), Some(b'\n' | b'\r')) {
                        c.inbuf.remove(0);
                    }
                    c.lines.push_back(line);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// Writes what it can; false if the connection is gone.
fn write(c: &mut Conn) -> bool {
    while !c.out.is_empty() {
        match c.stream.write(&c.out) {
            Ok(0) => return false,
            Ok(n) => {
                c.out.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

fn name_of(zone: &Zone, c: &Conn, pid: i16) -> Option<String> {
    let id = PlayerId(u16::try_from(pid).ok()?);
    c.names
        .get(&id)
        .cloned()
        .or_else(|| zone.players.get(id).map(|p| p.name.clone()))
}

fn msg_type(kind: u8) -> Option<&'static str> {
    Some(match kind {
        chat_type::ARENA => "ARENA",
        chat_type::PUBLIC_MACRO => "PUBM",
        chat_type::PUBLIC => "PUB",
        chat_type::FREQ | chat_type::ENEMY_FREQ => "FREQ",
        chat_type::PRIVATE | chat_type::REMOTE_PRIVATE => "PRIV",
        chat_type::SERVER_ERROR => "SYSOP",
        chat_type::CHANNEL => "CHAT",
        _ => return None,
    })
}

/// Translates one diverted game packet into lines for this client.
fn translate(zone: &Zone, me: PlayerId, c: &mut Conn, pkt: &[u8], out: &mut Vec<String>) {
    let Some(&kind) = pkt.first() else { return };
    match kind {
        0 => out.push(latin1(&pkt[1..])),
        s2c::kind::CHAT => {
            let Ok(s2c::S2CChat(ChatBody {
                kind, pid, text, ..
            })) = s2c::S2CChat::decode(pkt)
            else {
                return;
            };
            let text = latin1(&text);
            if kind == chat_type::REMOTE_PRIVATE && pid < 0 {
                // "(#squad)(sender)>msg" or "(sender)>msg"
                if let Some(rest) = text.strip_prefix("(#")
                    && let Some((squad, rest)) = rest.split_once(")(")
                    && let Some((sender, msg)) = rest.split_once(")>")
                {
                    out.push(format!("MSG:SQUAD:{squad}:{sender}:{msg}"));
                } else if let Some(rest) = text.strip_prefix('(')
                    && let Some((sender, msg)) = rest.split_once(")>")
                {
                    out.push(format!("MSG:PRIV:{sender}:{msg}"));
                }
                return;
            }
            let Some(mut ty) = msg_type(kind) else { return };
            // staff chat arrives as "name> text" in the sysop colour
            if kind == chat_type::SERVER_ERROR
                && pid < 0
                && let Some((n, _)) = text.split_once("> ")
                && zone.players.find_by_name(n).is_some()
            {
                ty = "MOD";
            }
            match name_of(zone, c, pid).filter(|_| pid >= 0) {
                Some(n) => out.push(format!("MSG:{ty}:{n}:{text}")),
                None => out.push(format!("MSG:{ty}:{text}")),
            }
        }
        s2c::kind::PLAYER_ENTERING => {
            let Ok(e) = s2c::PlayerEntering::decode(pkt) else {
                return;
            };
            for p in e.players {
                let Ok(id) = u16::try_from(p.pid) else {
                    continue;
                };
                let name = latin1(p.name.as_bytes());
                c.names.insert(PlayerId(id), name.clone());
                if PlayerId(id) != me {
                    out.push(format!("ENTERING:{name}:{}:{}", p.ship, p.freq));
                }
            }
        }
        s2c::kind::PLAYER_LEAVING => {
            if let Ok(l) = s2c::PlayerLeaving::decode(pkt)
                && let Some(n) = name_of(zone, c, l.pid)
            {
                out.push(format!("LEAVING:{n}"));
                if let Ok(id) = u16::try_from(l.pid) {
                    c.names.remove(&PlayerId(id));
                }
            }
        }
        s2c::kind::SHIP_CHANGE => {
            if let Ok(s) = s2c::ShipChange::decode(pkt)
                && let Some(n) = name_of(zone, c, s.pid)
            {
                out.push(format!("SHIPFREQCHANGE:{n}:{}:{}", s.ship, s.freq));
            }
        }
        s2c::kind::FREQ_CHANGE => {
            if let Ok(f) = s2c::FreqChange::decode(pkt)
                && let Some(n) = name_of(zone, c, f.pid)
            {
                let ship = u16::try_from(f.pid)
                    .ok()
                    .and_then(|id| zone.players.get(PlayerId(id)))
                    .map_or(8, |p| p.ship.0);
                out.push(format!("SHIPFREQCHANGE:{n}:{ship}:{}", f.freq));
            }
        }
        s2c::kind::KILL => {
            if let Ok(k) = s2c::Kill::decode(pkt)
                && let (Some(a), Some(b)) = (name_of(zone, c, k.killer), name_of(zone, c, k.killed))
            {
                out.push(format!("KILL:{a}:{b}:{}:{}", k.bounty, k.flags));
            }
        }
        s2c::kind::FLAG_VICTORY => {
            if let Ok(v) = s2c::FlagVictory::decode(pkt) {
                if v.freq < 0 {
                    out.push("MSG:ARENA:Flag game reset.".to_owned());
                } else {
                    out.push(format!(
                        "MSG:ARENA:Flag victory: freq {} won {} points.",
                        v.freq, v.points
                    ));
                }
            }
        }
        _ => {}
    }
}

fn chat_packet(kind: u8, target: i16, text: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(text.len() + 6);
    c2s::C2SChat(ChatBody {
        kind,
        sound: 0,
        pid: target,
        text: text.to_vec(),
    })
    .encode(&mut buf);
    buf
}

fn find_in_arena(zone: &Zone, me: PlayerId, name: &[u8]) -> Option<PlayerId> {
    let arena = zone.players.get(me)?.arena?;
    zone.players
        .find_by_name(&latin1(name))
        .filter(|t| t.arena == Some(arena) && t.status == PlayerStatus::Playing)
        .map(|t| t.id)
}

/// ASSS `MChat`.
fn on_send(zone: &mut Zone, p: PlayerId, rest: &[u8]) {
    if rest.len() > 500 {
        return;
    }
    let Some(i) = rest.iter().position(|&b| b == b':') else {
        return;
    };
    let sub = latin1(&rest[..i.min(9)]).to_ascii_uppercase();
    let t = &rest[i + 1..];
    let with_q = |t: &[u8]| [&b"?"[..], t].concat();
    let pkt = match sub.as_str() {
        "PUB" => chat_packet(chat_type::PUBLIC, 0, t),
        "PUBM" => chat_packet(chat_type::PUBLIC_MACRO, 0, t),
        "CMD" => chat_packet(chat_type::PUBLIC, 0, &with_q(t)),
        "PRIV" | "PRIVCMD" => {
            let Some(j) = t.iter().position(|&b| b == b':') else {
                return;
            };
            let (name, msg) = (&t[..j.min(23)], &t[j + 1..]);
            let msg = if sub == "PRIVCMD" {
                with_q(msg)
            } else {
                msg.to_vec()
            };
            match find_in_arena(zone, p, name) {
                Some(target) => chat_packet(chat_type::PRIVATE, target.0 as i16, &msg),
                None => {
                    let mut r = vec![b':'];
                    r.extend_from_slice(name);
                    r.push(b':');
                    r.extend_from_slice(&msg);
                    chat_packet(chat_type::REMOTE_PRIVATE, 0, &r)
                }
            }
        }
        "FREQ" => {
            let Some(j) = t.iter().position(|&b| b == b':') else {
                return;
            };
            let freq: Option<u16> = latin1(&t[..j.min(23)]).trim().parse().ok();
            let msg = &t[j + 1..];
            let Some(me) = zone.players.get(p) else {
                return;
            };
            let (my_freq, arena) = (me.freq, me.arena);
            match freq {
                Some(f) if f == my_freq => chat_packet(chat_type::FREQ, 0, msg),
                Some(f) => {
                    // a message to another team goes "through" one of its players
                    let Some(t) = zone
                        .players
                        .iter()
                        .find(|o| o.arena == arena && o.freq == f && o.id != p)
                        .map(|o| o.id)
                    else {
                        return;
                    };
                    chat_packet(chat_type::ENEMY_FREQ, t.0 as i16, msg)
                }
                None => return,
            }
        }
        "CHAT" => chat_packet(chat_type::CHANNEL, 0, t),
        "MOD" => chat_packet(chat_type::PUBLIC, 0, &[&b"\\"[..], t].concat()),
        _ => return,
    };
    zone.handle_packet(p, &pkt);
}

/// ASSS `MLogin`: `version[;client]:name:password`.
fn on_login(zone: &mut Zone, p: PlayerId, rest: &[u8]) {
    if zone.players.get(p).map(|x| x.status) != Some(PlayerStatus::Connected) {
        tracing::warn!(target: "chatnet", pid = p.0, "login request from wrong stage");
        return;
    }
    let Some(i) = rest.iter().position(|&b| b == b':') else {
        return;
    };
    let vers = latin1(&rest[..i.min(63)]);
    let rest = &rest[i + 1..];
    let Some(j) = rest.iter().position(|&b| b == b':') else {
        return;
    };
    let mut name: Vec<u8> = rest[..j.min(31)]
        .iter()
        .map(|&b| if (32..=126).contains(&b) { b } else { b'_' })
        .collect();
    if let Some(first) = name.first_mut()
        && !first.is_ascii_alphanumeric()
    {
        *first = b'_';
    }
    let password = &rest[j + 1..];
    let client_version = vers
        .split(';')
        .next()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(0);
    if let Some(pl) = zone.players.get_mut(p)
        && let Some((_, client)) = vers.split_once(';')
    {
        pl.client_name = format!("chat: {client}");
    }
    let mut buf = Vec::with_capacity(Login::VIE_LEN);
    Login {
        kind: c2s::kind::LOGIN,
        flags: 0,
        name: FixedStr::new(&name),
        password: FixedStr::new(&password[..password.len().min(31)]),
        machine_id: 101,
        connect_type: 0,
        timezone_bias: 0,
        unknown1: 0,
        client_version,
        field444: 0,
        field555: 0,
        permission_id: 101,
        reserved: [0; 12],
        cont_id: None,
    }
    .encode(&mut buf);
    zone.handle_packet(p, &buf);
}

fn dispatch(zone: &mut Zone, p: PlayerId, line: &[u8]) {
    let (ty, rest) = match line.iter().position(|&b| b == b':') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, &b""[..]),
    };
    match ty {
        b"LOGIN" => on_login(zone, p, rest),
        b"GO" => {
            let name = latin1(rest);
            let name = if name.is_empty() {
                "0".to_owned()
            } else {
                name
            };
            let force_filter = zone.cfg.global().get_int("Chat", "ForceFilter", 0) != 0;
            let filter = zone.players.get(p).is_some_and(|x| x.obscenity_filter) || force_filter;
            super::arenaman::go(
                zone,
                p,
                super::arenaman::GoRequest {
                    name,
                    ship: Ship::SPEC,
                    resolution: (0, 0),
                    want_all_lvz: false,
                    accept_audio: false,
                    obscenity_filter: filter,
                },
            );
        }
        b"LEAVE" => super::arenaman::leave_arena(zone, p),
        b"SEND" => on_send(zone, p, rest),
        b"CHANGEFREQ" => {
            if let Ok(f) = latin1(rest).trim().parse::<i16>() {
                let mut buf = vec![c2s::kind::SET_FREQ];
                buf.extend_from_slice(&f.to_le_bytes());
                zone.handle_packet(p, &buf);
            }
        }
        _ => {}
    }
}

fn iterate(zone: &mut Zone) -> bool {
    accept(zone);
    let now = zone.now();
    let (pkts, drops) = zone.take_text_output();
    let Some(mut cn) = zone.res.remove::<ChatNet>() else {
        return true;
    };
    // translate what the zone sent
    let mut lines = Vec::new();
    for (pid, pkt) in pkts {
        if let Some(c) = cn.conns.get_mut(&pid) {
            lines.clear();
            translate(zone, pid, c, &pkt, &mut lines);
            for l in &lines {
                let mut b = to_latin1(l);
                b.truncate(MAX_LINE - 2);
                c.out.extend_from_slice(&b);
                c.out.push(b'\n');
                c.last_send = now;
            }
        }
    }
    for pid in drops {
        if let Some(c) = cn.conns.get_mut(&pid) {
            c.closing = true;
        }
    }
    let mut to_kick = Vec::new();
    let mut to_run = Vec::new();
    let mut gone = Vec::new();
    for (pid, c) in &mut cn.conns {
        let alive = zone
            .players
            .get(*pid)
            .is_some_and(|p| p.kind == ClientKind::Chat);
        if !alive {
            gone.push(*pid);
            continue;
        }
        if !c.closing && !read(c, now) {
            to_kick.push(*pid);
            c.closing = true;
        }
        if !c.closing
            && !c.lines.is_empty()
            && now.since(c.last_proc) > cn.delay
            && let Some(l) = c.lines.pop_front()
        {
            c.last_proc = now;
            to_run.push((*pid, l));
        }
        if now.since(c.last_send) > NOOP_AFTER && now.since(c.last_recv) > NOOP_AFTER {
            c.out.extend_from_slice(b"NOOP\n");
            c.last_send = now;
        }
        if !write(c) || c.out.len() > MAX_PENDING_OUT {
            if !c.closing {
                to_kick.push(*pid);
            }
            c.closing = true;
            c.out.clear();
        }
        if c.closing && c.out.is_empty() {
            let _ = c.stream.shutdown(std::net::Shutdown::Both);
        }
    }
    for pid in &gone {
        cn.conns.remove(pid);
        zone.set_text_player(*pid, false);
        tracing::debug!(target: "chatnet", pid = pid.0, "connection closed");
    }
    zone.res.insert(cn);
    for pid in to_kick {
        tracing::info!(target: "chatnet", pid = pid.0, "disconnected");
        super::core::kick(zone, pid);
    }
    for (pid, line) in to_run {
        dispatch(zone, pid, &line);
    }
    true
}

/// The `chatnet` module.
pub struct ChatNetModule;

impl Module for ChatNetModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let delay = zone.cfg.global().get_int("Net", "ChatMessageDelay", 20);
        zone.res.insert(ChatNet {
            delay: u64::try_from(delay).unwrap_or(20),
            ..ChatNet::default()
        });
        // listen only once the server runs, so building a zone (tests, tools)
        // doesn't take the port
        zone.on::<ZoneStarted>(me, Scope::Global, |z, _| start(z));
        zone.set_timer(me, 1, 1, iterate);
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        if let Some(cn) = zone.res.remove::<ChatNet>() {
            for (pid, c) in cn.conns {
                let _ = c.stream.shutdown(std::net::Shutdown::Both);
                zone.set_text_player(pid, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_types() {
        assert_eq!(msg_type(chat_type::ENEMY_FREQ), Some("FREQ"));
        assert_eq!(msg_type(chat_type::CHANNEL), Some("CHAT"));
        assert_eq!(msg_type(200), None);
    }
}
