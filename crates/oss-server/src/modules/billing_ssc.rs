//! `billing_ssc`: client for an SSC ("SubSpace Central") user database
//! server, the biller (ASSS `billing_ssc.c`).
//!
//! The zone connects to `Billing:IP`/`Billing:Port` like a SubSpace client
//! (VIE encryption, reliable packets) and identifies itself with
//! `S2B_ServerConnect`. The biller never answers that, so any packet from it,
//! or five seconds without being dropped, counts as logged in. From then on:
//!
//! - logins are checked by the biller (this module is the zone's [`Auth`]);
//!   while it is down players get in unauthenticated with a `^` before their
//!   name, unless a [`PasswordCache`] knows them;
//! - logoffs carry the player's final score, but only when it differs from
//!   the score this module last recorded -- and leaving a public arena
//!   records it, so logging out of one normally sends the short 15-byte
//!   logoff with no score at all. ASSS does the same: its `update_score`
//!   compares against `p->pkt`, which `PA_LEAVEARENA` has just matched, and
//!   neither server ever sends `S2B_UserScore`. A biller's score realm
//!   therefore fills from non-public arenas, or when flags carried and flag
//!   pickups disagree. Run against `oss-biller` on 2026-11-13: a kill in
//!   arena `0` reached no `scores` row, the same kill in arena `duel` did;
//! - chat channels (`;channel;text`), remote private messages to players in
//!   other zones and squad messages, `?commands` the zone doesn't know
//!   (including `?chat=` with the `Billing:LocalChats`/`StaffChats`
//!   rewriting) and banners go to the biller;
//! - the biller can kick players, reset public scores and send messages.
//!
//! The connection is made through a [`BillerConnector`]; the default one
//! opens a UDP socket with the VIE handshake. Private builds can register
//! their own (for a different encryption) and tests use an in-memory link.
//!
//! Differences from ASSS (logged in the decision notes): the first-login date
//! in `?usage` is printed month-day-year rather than ASSS's month-year-day,
//! and biller packets forwarded to a player (S2C `0x33` login text) are sent
//! whole, where ASSS drops their last byte.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Instant;

use oss_net::{Config, Connection, Event as NetEvent, VieClientHandshake};
use oss_proto::billing::{
    BillerLogin, BillerToServer, PlayerScore, ServerToBiller, UserLogin, login_result,
};
use oss_proto::common::chat_type;
use oss_proto::s2c;
use oss_proto::wire::{FixedStr, Raw};

use crate::commands::{Invocation, Target};
use crate::config::{latin1, to_latin1};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::time::Ticks;
use crate::zone::Zone;

use super::banners::{BANNER_LEN, BannerSet, Banners};
use super::chat::{Chat, ChatMessage, mask_type, message, send_remote_priv};
use super::core::{Action, Auth, AuthData, LoginRequest, PlayerAction, auth_code};
use super::persist::Interval;
use super::stats::{Stats, stat};

/// Most logins waiting for the biller before new ones are told to retry.
pub const MAX_PENDING_AUTHS: i32 = 15;
/// Most recently interrupted logins before new ones are told to retry.
pub const MAX_INTERRUPTED_AUTHS: i32 = 20;
/// Longest text field the biller accepts, without its NUL.
const MAX_TEXT: usize = 249;

// ---- the link -------------------------------------------------------------

/// Something that happened on a biller link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// The connection is up (handshake done).
    Connected,
    /// A packet from the biller.
    Packet(Vec<u8>),
    /// The connection is gone.
    Disconnected,
}

/// A connection to the biller, driven from the game thread.
pub trait BillerLink: Send {
    /// Does pending I/O and appends what happened.
    fn poll(&mut self, events: &mut Vec<LinkEvent>);
    /// Queues a packet.
    fn send(&mut self, packet: &[u8], reliable: bool);
    /// Sends what is queued and closes the connection.
    fn close(&mut self);
}

/// Opens biller links (ASSS `MakeClientConnection` with `enc-vie-client`).
pub trait BillerConnector: Send + Sync {
    /// Starts connecting to `addr`; `None` if that failed at once.
    fn connect(&self, addr: SocketAddr) -> Option<Box<dyn BillerLink>>;
}

/// The default connector: UDP with VIE encryption.
pub struct UdpConnector;

impl BillerConnector for UdpConnector {
    fn connect(&self, addr: SocketAddr) -> Option<Box<dyn BillerLink>> {
        let local: SocketAddr = if addr.is_ipv4() {
            ([0, 0, 0, 0], 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = UdpSocket::bind(local).ok()?;
        socket.set_nonblocking(true).ok()?;
        let start = Instant::now();
        let seed = {
            let mut b = [0u8; 4];
            oss_net::opensec::os_entropy()(&mut b);
            u32::from_le_bytes(b)
        };
        let conn = Connection::client(Config::client(), Box::new(VieClientHandshake::new(seed)), 0);
        let mut link = UdpLink {
            socket,
            peer: addr,
            conn,
            start,
            buf: vec![0; 2048],
        };
        link.transmit();
        Some(Box::new(link))
    }
}

struct UdpLink {
    socket: UdpSocket,
    peer: SocketAddr,
    conn: Connection,
    start: Instant,
    buf: Vec<u8>,
}

impl UdpLink {
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn transmit(&mut self) {
        let now = self.now();
        self.conn.flush(now);
        while let Some(d) = self.conn.poll_transmit() {
            if let Err(e) = self.socket.send_to(&d, self.peer) {
                tracing::trace!(target: "billing_ssc", "send failed: {e}");
            }
        }
    }
}

impl BillerLink for UdpLink {
    fn poll(&mut self, events: &mut Vec<LinkEvent>) {
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, from)) if from == self.peer => {
                    let now = self.now();
                    let data = self.buf[..n].to_vec();
                    self.conn.handle_datagram(now, &data);
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // e.g. ICMP port unreachable on Windows: the tick timeout drops us
                Err(_) => break,
            }
        }
        let now = self.now();
        self.conn.tick(now);
        self.transmit();
        while let Some(ev) = self.conn.poll_event() {
            match ev {
                NetEvent::Connected => events.push(LinkEvent::Connected),
                NetEvent::Packet(p) => events.push(LinkEvent::Packet(p)),
                NetEvent::Disconnected(_) => events.push(LinkEvent::Disconnected),
                _ => {}
            }
        }
    }

    fn send(&mut self, packet: &[u8], reliable: bool) {
        self.conn.send(packet, reliable);
    }

    fn close(&mut self) {
        self.transmit();
        self.conn.disconnect();
        self.transmit();
    }
}

// ---- interfaces -------------------------------------------------------------

/// Biller connection status (ASSS `billing_state_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingStatus {
    /// Not configured, or turned off.
    Disabled,
    /// Configured but not connected.
    Down,
    /// Connected and logged in.
    Up,
}

/// The billing interface (ASSS `Ibilling`).
pub trait Billing: Send + Sync {
    /// Connection status.
    fn status(&self, zone: &Zone) -> BillingStatus;
    /// The identity data the biller sent, if any.
    fn identity(&self, zone: &Zone) -> Option<Vec<u8>>;
}

/// Result of a [`PasswordCache`] check (ASSS `pwcache`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheResult {
    /// The name is known and the password matches.
    Match,
    /// The name is known and the password doesn't match.
    Mismatch,
    /// Unknown name.
    NotFound,
}

/// Remembers passwords the biller accepted, so players can log in while it is
/// down (ASSS `Ipwcache`). Optional.
pub trait PasswordCache: Send + Sync {
    /// Checks a name and password.
    fn check(&self, zone: &mut Zone, name: &str, password: &str) -> CacheResult;
    /// Records a password the biller accepted.
    fn set(&self, zone: &mut Zone, name: &str, password: &str);
}

// ---- state ------------------------------------------------------------------

/// Connection state (ASSS `s_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected yet.
    NoSocket,
    /// Handshake in progress.
    Connecting,
    /// Connected, waiting for any sign the login worked.
    WaitLogin,
    /// Logged in.
    LoggedIn,
    /// Waiting to retry.
    Retry,
    /// Login failed.
    LoginFailed,
    /// Turned off (no `Billing:IP`, or `?userdbadm drop`).
    Disabled,
}

struct BillingState {
    state: LinkState,
    last_event: Ticks,
    link: Option<Box<dyn BillerLink>>,
    pending_auths: i32,
    interrupted_auths: i32,
    interrupted_damp: Ticks,
    identity: Option<Vec<u8>>,
    last_banner: Option<Ticks>,
    retry_ticks: u64,
}

impl Default for BillingState {
    fn default() -> Self {
        Self {
            state: LinkState::NoSocket,
            last_event: Ticks(0),
            link: None,
            pending_auths: 0,
            interrupted_auths: 0,
            interrupted_damp: Ticks(0),
            identity: None,
            last_banner: None,
            retry_ticks: 18000,
        }
    }
}

#[derive(Debug, Default)]
struct LoginData {
    name: String,
    password: String,
}

#[derive(Debug, Default)]
struct BillingPlayer {
    billing_id: u32,
    usage: u32,
    known: bool,
    demographics_set: bool,
    login: Option<LoginData>,
    first_used: String,
    saved_score: PlayerScore,
}

fn st(zone: &mut Zone) -> &mut BillingState {
    zone.res.get_or_default::<BillingState>()
}

fn pdata(zone: &mut Zone, p: PlayerId) -> Option<&mut BillingPlayer> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<BillingPlayer>())
}

fn known(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<BillingPlayer>())
        .is_some_and(|d| d.known)
}

/// The connection state.
pub fn link_state(zone: &Zone) -> LinkState {
    zone.res
        .get::<BillingState>()
        .map_or(LinkState::NoSocket, |s| s.state)
}

/// Logins waiting for the biller.
pub fn pending_auths(zone: &Zone) -> i32 {
    zone.res
        .get::<BillingState>()
        .map_or(0, |s| s.pending_auths)
}

/// The biller's user id for a player, if the biller knows them.
pub fn user_id(zone: &Zone, p: PlayerId) -> Option<u32> {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<BillingPlayer>())
        .filter(|d| d.known)
        .map(|d| d.billing_id)
}

fn send(zone: &mut Zone, pkt: &[u8], reliable: bool) {
    if let Some(l) = st(zone).link.as_mut() {
        l.send(pkt, reliable);
    }
}

fn send_s2b(zone: &mut Zone, pkt: &ServerToBiller) {
    send(zone, &pkt.to_bytes(), true);
}

/// `s` as a NUL-terminated field of `N` bytes (ASSS `astrncpy`).
fn fixed<const N: usize>(s: &[u8]) -> FixedStr<N> {
    FixedStr::new(&s[..s.len().min(N.saturating_sub(1))])
}

/// `s` cut to `n` bytes.
fn cut(mut v: Vec<u8>, n: usize) -> Vec<u8> {
    v.truncate(n);
    v
}

/// C `delimcpy`: copies up to `size - 1` bytes of `s` before `delim` and
/// returns them with the rest after `delim` (`None` if there's no `delim`).
fn delimcpy(s: &[u8], size: usize, delim: u8) -> (Vec<u8>, Option<&[u8]>) {
    match s.iter().position(|&b| b == delim) {
        Some(i) => (cut(s[..i].to_vec(), size - 1), Some(&s[i + 1..])),
        None => (cut(s.to_vec(), size - 1), None),
    }
}

fn announce(zone: &mut Zone, text: &str) {
    let Some(chat) = zone.iface::<dyn Chat>() else {
        return;
    };
    for a in zone.arenas.ids() {
        chat.send_arena_message(zone, a, text);
    }
}

fn drop_connection(zone: &mut Zone, new_state: LinkState) {
    if st(zone).state == LinkState::LoggedIn {
        announce(zone, "Notice: Connection to user database server lost");
    }
    for p in zone.players.ids() {
        if let Some(d) = zone
            .players
            .get_mut(p)
            .and_then(|x| x.ext.get_mut::<BillingPlayer>())
        {
            d.known = false;
        }
    }
    let now = zone.now();
    let s = st(zone);
    if let Some(mut l) = s.link.take() {
        // ASSS sends this unreliably: reliable data isn't sent after a drop
        l.send(&ServerToBiller::ServerDisconnect.to_bytes(), false);
        l.close();
    }
    s.state = new_state;
    s.last_event = now;
}

// ---- authentication ---------------------------------------------------------

fn not_found(name: &str) -> AuthData {
    let n = format!("^{name}");
    let mut ad = AuthData::ok(&n);
    ad.send_name = n.chars().take(19).collect();
    ad
}

fn cache_done(zone: &mut Zone, p: PlayerId, result: CacheResult) {
    let Some(login) = pdata(zone, p).and_then(|d| d.login.take()) else {
        tracing::warn!(target: "billing_ssc", pid = p.0, "unexpected pwcache response");
        return;
    };
    let ad = match result {
        CacheResult::Match => {
            let mut ad = AuthData::ok(&login.name);
            ad.authenticated = true;
            ad
        }
        CacheResult::NotFound => {
            if let Some(d) = pdata(zone, p) {
                d.known = false;
            }
            not_found(&login.name)
        }
        CacheResult::Mismatch => AuthData {
            code: auth_code::BAD_PASSWORD,
            ..AuthData::default()
        },
    };
    super::core::auth_done(zone, p, ad);
}

struct BillingAuth;

impl Auth for BillingAuth {
    fn authenticate(&self, zone: &mut Zone, p: PlayerId, login: &LoginRequest) {
        let lp = &login.packet;
        let name = latin1(lp.name.as_bytes());
        let password = latin1(lp.password.as_bytes());
        let cache = zone.iface::<dyn PasswordCache>();
        let Some(d) = pdata(zone, p) else { return };
        d.known = false;
        d.login = Some(LoginData {
            name: name.chars().take(23).collect(),
            password: if cache.is_some() {
                password.chars().take(23).collect()
            } else {
                String::new()
            },
        });
        if st(zone).state == LinkState::LoggedIn {
            let s = st(zone);
            if s.pending_auths < MAX_PENDING_AUTHS && s.interrupted_auths < MAX_INTERRUPTED_AUTHS {
                let pl = zone.players.get(p);
                let ip = match pl.and_then(|x| x.addr) {
                    Some(SocketAddr::V4(a)) => a.ip().octets(),
                    _ => [0; 4],
                };
                let extra = match (pl.map(|x| x.kind), lp.cont_id) {
                    (Some(ClientKind::Continuum), Some(c)) => c.to_vec(),
                    _ => Vec::new(),
                };
                let pkt = ServerToBiller::UserLogin(UserLogin {
                    make_new: lp.flags,
                    ip,
                    name: fixed(lp.name.as_bytes()),
                    password: fixed(lp.password.as_bytes()),
                    connection_id: u32::from(p.0),
                    machine_id: lp.machine_id,
                    timezone: lp.timezone_bias,
                    timezone_high: if lp.timezone_bias < 0 { 0xFFFF } else { 0 },
                    unknown: 0,
                    sysop: 0,
                    client_version: lp.client_version,
                    extra,
                });
                send_s2b(zone, &pkt);
                if let Some(d) = pdata(zone, p) {
                    d.known = true;
                }
                st(zone).pending_auths += 1;
            } else {
                // tell the player to try again later
                if let Some(d) = pdata(zone, p) {
                    d.login = None;
                }
                super::core::auth_done(
                    zone,
                    p,
                    AuthData {
                        code: auth_code::SERVER_BUSY,
                        ..AuthData::default()
                    },
                );
            }
        } else if let Some(c) = cache {
            let r = c.check(zone, &name, &password);
            cache_done(zone, p, r);
        } else {
            cache_done(zone, p, CacheResult::NotFound);
        }
    }
}

fn current_score(zone: &Zone, p: PlayerId) -> PlayerScore {
    let Some(pl) = zone.players.get(p) else {
        return PlayerScore::default();
    };
    PlayerScore {
        kills: pl.score.wins as u16,
        deaths: pl.score.losses as u16,
        flags: pl.flags_carried as u16,
        points: pl.score.kill_points as u32,
        flag_points: pl.score.flag_points as u32,
    }
}

fn on_player_action(zone: &mut Zone, ev: &PlayerAction) {
    let p = ev.player;
    match ev.action {
        Action::Disconnect => {
            let Some(d) = pdata(zone, p) else { return };
            let had_login = d.login.take().is_some();
            let was_known = d.known;
            if had_login && was_known {
                // disconnected while waiting for the biller
                let s = st(zone);
                s.pending_auths -= 1;
                s.interrupted_auths += 1;
            }
            if was_known {
                let score = current_score(zone, p);
                let changed = pdata(zone, p).is_some_and(|d| {
                    let c = d.saved_score != score;
                    d.saved_score = score;
                    c
                });
                // without the score when it didn't change (ASSS sends 15 bytes)
                let pkt = ServerToBiller::UserLogoff {
                    connection_id: u32::from(p.0),
                    reason: 0,
                    latency: 0,
                    ping: 0,
                    loss_s2c: 0,
                    loss_c2s: 0,
                    score: changed.then_some(score),
                };
                tracing::debug!(
                    target: "billing_ssc",
                    pid = p.0,
                    with_score = changed,
                    kills = score.kills,
                    deaths = score.deaths,
                    points = score.points,
                    "logging a player off at the user database server"
                );
                send_s2b(zone, &pkt);
            }
        }
        Action::LeaveArena => {
            let public = ev
                .arena
                .and_then(|a| zone.arenas.get(a))
                .is_some_and(|a| a.is_public());
            if !public {
                return;
            }
            let Some(stats) = zone.iface::<dyn Stats>() else {
                return;
            };
            let g = |s| stats.get(zone, p, s);
            let score = PlayerScore {
                points: g(stat::KILL_POINTS) as u32,
                flag_points: g(stat::FLAG_POINTS) as u32,
                kills: g(stat::KILLS) as u16,
                deaths: g(stat::DEATHS) as u16,
                flags: g(stat::FLAG_PICKUPS) as u16,
            };
            if let Some(d) = pdata(zone, p) {
                d.saved_score = score;
            }
        }
        _ => {}
    }
}

// ---- chat and commands going to the biller ----------------------------------

fn on_chat(zone: &mut Zone, ev: &ChatMessage) {
    let p = ev.from;
    if !known(zone, p) {
        return;
    }
    let text = to_latin1(&ev.text);
    if ev.kind == chat_type::CHANNEL {
        // "text" or "channel;text" (a name works too, if the biller allows)
        let (channel, body) = match text.iter().position(|&b| b == b';') {
            Some(i) if i < 10 => {
                let (c, rest) = delimcpy(&text, 32, b';');
                (c, rest.unwrap_or_default().to_vec())
            }
            _ => (b"1".to_vec(), text),
        };
        send_s2b(
            zone,
            &ServerToBiller::UserChannelChat {
                connection_id: u32::from(p.0),
                channel: fixed(&channel),
                text: cut(body, MAX_TEXT),
            },
        );
    } else if ev.kind == chat_type::REMOTE_PRIVATE && ev.to.is_none() {
        // only the ones the zone couldn't deliver itself: ":target:text"
        let (dest, rest) = delimcpy(text.get(1..).unwrap_or_default(), 32, b':');
        let (Some(b':'), Some(rest)) = (text.first().copied(), rest) else {
            tracing::warn!(target: "billing_ssc", pid = p.0, "malformed remote private message");
            return;
        };
        let name = zone
            .players
            .get(p)
            .map(|x| to_latin1(&x.name))
            .unwrap_or_default();
        let mut t = Vec::with_capacity(dest.len() + name.len() + rest.len() + 5);
        t.push(b':');
        t.extend_from_slice(&dest);
        t.extend_from_slice(b":(");
        t.extend_from_slice(&name);
        t.extend_from_slice(b")>");
        t.extend_from_slice(rest);
        send_s2b(
            zone,
            &ServerToBiller::UserRemotePrivate {
                // ASSS: a connection id >= 0 means a global broadcast
                connection_id: u32::MAX,
                group_id: 1,
                kind: 2,
                sound: ev.sound,
                text: cut(t, MAX_TEXT),
            },
        );
    }
}

/// ASSS `findchat`: is `chat` in the `\t:;,`-separated `list`?
fn find_chat(chat: &str, list: &str) -> bool {
    list.split(['\t', ':', ';', ','])
        .filter(|t| !t.is_empty())
        .any(|t| {
            let t: String = t.chars().take(31).collect();
            t.eq_ignore_ascii_case(chat)
        })
}

/// ASSS `rewrite_chat_command`: marks local and staff chats in `?chat=` with
/// the configured secret prefixes so the biller keeps them apart.
pub fn rewrite_chat_command(
    line: &str,
    local: Option<(&str, &str)>,
    staff: Option<(&str, &str)>,
) -> String {
    const LIMIT: usize = 250 - 64;
    let mut out = String::from("?chat=");
    let mut rest = line.get(5..);
    while let Some(r) = rest {
        if out.len() >= LIMIT {
            break;
        }
        let (name, next) = match r.split_once(',') {
            Some((n, x)) => (n, Some(x)),
            None => (r, None),
        };
        rest = next;
        let orig: String = name.chars().take(31).collect();
        let mut chat = orig.as_str();
        if let Some(i) = chat.rfind('/') {
            chat = &chat[..i];
        }
        let chat = chat.trim_end_matches(' ');
        if chat.is_empty() {
            continue;
        }
        if let Some((list, prefix)) = local
            && find_chat(chat, list)
        {
            out.extend(format!("$l${prefix}|").chars().take(31));
        }
        if let Some((list, prefix)) = staff
            && find_chat(chat, list)
        {
            out.extend(format!("$s${prefix}|").chars().take(31));
        }
        out.push_str(&orig);
        if rest.is_some() {
            out.push(',');
        }
    }
    out
}

fn c_default(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    if !known(zone, p) {
        return;
    }
    if !matches!(inv.target, Target::Arena(_)) {
        tracing::debug!(target: "billing_ssc", pid = p.0, "unknown command with bad target: {}", inv.params);
        return;
    }
    if !super::chat::allowed(zone, p, mask_type::BCOMMAND) {
        return;
    }
    let line = inv.params;
    let lower = line.get(..5).map(str::to_ascii_lowercase);
    let text = if lower.as_deref() == Some("chat ") || lower.as_deref() == Some("chat=") {
        let g = zone.cfg.global().clone();
        let local = g.get("Billing", "LocalChats").map(|l| {
            (
                l.to_owned(),
                g.get("Billing", "LocalChatPrefix").unwrap_or("").to_owned(),
            )
        });
        let can_staff = zone
            .iface::<dyn super::capman::Capabilities>()
            .is_some_and(|c| c.has_capability(zone, p, super::capman::CAP_SEND_MOD_CHAT));
        let staff = g
            .get("Billing", "StaffChats")
            .filter(|_| can_staff)
            .map(|l| {
                (
                    l.to_owned(),
                    g.get("Billing", "StaffChatPrefix").unwrap_or("").to_owned(),
                )
            });
        rewrite_chat_command(
            line,
            local.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            staff.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
        )
    } else {
        format!("?{line}")
    };
    send_s2b(
        zone,
        &ServerToBiller::UserCommand {
            connection_id: u32::from(p.0),
            text: cut(to_latin1(&text), MAX_TEXT),
        },
    );
}

fn target_or_self(inv: &Invocation<'_>) -> PlayerId {
    match inv.target {
        Target::Player(t) => t,
        _ => inv.sender,
    }
}

fn hms(secs: u64) -> String {
    let mins = secs / 60;
    format!("{:5}:{:02}:{:02}", mins / 60, mins % 60, secs % 60)
}

fn c_usage(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    let t = target_or_self(inv);
    let now = zone.now();
    let Some(tp) = zone.players.get(t) else {
        return;
    };
    let name = tp.name.clone();
    let secs = now.since(tp.connected_at) / 100;
    let info = tp
        .ext
        .get::<BillingPlayer>()
        .filter(|d| d.known)
        .map(|d| (d.usage, d.first_used.clone()));
    match info {
        Some((usage, first)) => {
            if t != p {
                message(zone, p, &format!("usage: {name}:"));
            }
            message(zone, p, &format!("session: {}", hms(secs)));
            message(
                zone,
                p,
                &format!("  total: {}", hms(secs + u64::from(usage))),
            );
            message(zone, p, &format!("first played: {first}"));
        }
        None => message(zone, p, &format!("usage unknown for {name}")),
    }
}

fn c_userid(zone: &mut Zone, inv: &Invocation<'_>) {
    let t = target_or_self(inv);
    let name = zone
        .players
        .get(t)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    let text = match user_id(zone, t) {
        Some(id) => format!("{name} has user id {id}"),
        None => format!("user id unknown for {name}"),
    };
    message(zone, inv.sender, &text);
}

fn c_userdbadm(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    match inv.params {
        "drop" => {
            drop_connection(zone, LinkState::Disabled);
            message(zone, p, "user database connection disabled");
        }
        "connect" => {
            let s = st(zone);
            if matches!(
                s.state,
                LinkState::LoginFailed | LinkState::Disabled | LinkState::Retry
            ) {
                s.state = LinkState::NoSocket;
                message(zone, p, "user database server connection reactivated");
            } else {
                message(zone, p, "user database server connection already active");
            }
        }
        _ => {
            let s = st(zone);
            let t = match s.state {
                LinkState::NoSocket => "not connected yet",
                LinkState::Connecting => "connecting",
                LinkState::WaitLogin => "waiting for login response",
                LinkState::LoggedIn => "logged in",
                LinkState::Retry => "waiting to retry",
                LinkState::LoginFailed => "disabled (login failed)",
                LinkState::Disabled => "disabled (by user)",
            };
            let text = format!(
                "user database status: {t}  pending auths: {}",
                s.pending_auths
            );
            message(zone, p, &text);
        }
    }
}

fn on_demographics(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if pkt.len().saturating_sub(1) > 765 {
        tracing::warn!(target: "billing_ssc", pid = p.0, len = pkt.len(), "invalid demographics packet");
        return;
    }
    let Some(d) = pdata(zone, p) else { return };
    if d.demographics_set {
        tracing::warn!(target: "billing_ssc", pid = p.0, "duplicate demographics packet");
    } else if d.known {
        d.demographics_set = true;
        send_s2b(
            zone,
            &ServerToBiller::UserDemographics {
                connection_id: u32::from(p.0),
                data: pkt.get(1..).unwrap_or_default().to_vec(),
            },
        );
    }
}

fn on_banner(zone: &mut Zone, ev: &BannerSet) {
    let p = ev.player;
    let now = zone.now();
    let last = st(zone).last_banner;
    // at most one banner every 0.2 s gets to the biller; the rest are dropped
    if known(zone, p) && last.is_none_or(|l| now.since(l) > 20) {
        send_s2b(
            zone,
            &ServerToBiller::UserBanner {
                connection_id: u32::from(p.0),
                banner: Raw(ev.banner),
            },
        );
        st(zone).last_banner = Some(now);
    }
}

// ---- packets from the biller ------------------------------------------------

fn player_for(zone: &Zone, connection_id: u32) -> Option<PlayerId> {
    let id = PlayerId(u16::try_from(connection_id).ok()?);
    zone.players.get(id).map(|_| id)
}

fn process_user_login(zone: &mut Zone, data: &[u8], pkt: BillerLogin) {
    if data.len() < 178 {
        tracing::warn!(target: "billing_ssc", len = data.len(), "invalid login packet");
        return;
    }
    let Some(p) = player_for(zone, pkt.connection_id) else {
        tracing::warn!(target: "billing_ssc", pid = pkt.connection_id, "biller sent player auth response for unknown pid");
        return;
    };
    let Some(login) = pdata(zone, p).and_then(|d| d.login.take()) else {
        tracing::warn!(target: "billing_ssc", pid = p.0, "unexpected player auth response");
        return;
    };
    let ok = matches!(
        pkt.result,
        login_result::OK | login_result::DEMO | login_result::ASK_DEMOGRAPHICS
    );
    tracing::debug!(
        target: "billing_ssc",
        pid = p.0,
        result = pkt.result,
        user_id = pkt.user_id,
        seconds_played = pkt.seconds_played,
        kills = pkt.score.map(|s| s.kills),
        deaths = pkt.score.map(|s| s.deaths),
        points = pkt.score.map(|s| s.points),
        "user database server answered a login"
    );
    let ad = if ok {
        let [y, mo, d, h, mi, s] = pkt.first_login;
        if let Some(bd) = pdata(zone, p) {
            bd.first_used = format!("{mo:02}-{d:02}-{y:04} {h:02}:{mi:02}:{s:02}");
            bd.usage = pkt.seconds_played;
            bd.billing_id = pkt.user_id;
            bd.saved_score = PlayerScore::default();
        }
        let name = latin1(pkt.name.as_bytes());
        let ad = AuthData {
            code: auth_code::OK,
            demo_data: pkt.result == login_result::ASK_DEMOGRAPHICS,
            authenticated: true,
            send_name: name.chars().take(19).collect(),
            name,
            squad: latin1(pkt.squad.as_bytes()),
            custom_text: String::new(),
        };
        if pkt.banner.0.iter().any(|&b| b != 0)
            && let Some(b) = zone.iface::<dyn Banners>()
        {
            let banner: [u8; BANNER_LEN] = pkt.banner.0;
            b.set_banner(zone, p, &banner, true);
        }
        if let Some(c) = zone.iface::<dyn PasswordCache>() {
            c.set(zone, &login.name, &login.password);
        }
        ad
    } else {
        AuthData {
            code: match pkt.result {
                login_result::NEW_USER => auth_code::NEW_NAME,
                login_result::INVALID_PASSWORD => auth_code::BAD_PASSWORD,
                login_result::BANNED => auth_code::LOCKED_OUT,
                login_result::NO_NEW_CONNECTIONS => auth_code::NO_NEW_CONN,
                login_result::BAD_NAME => auth_code::BAD_NAME,
                login_result::BUSY => auth_code::SERVER_BUSY,
                _ => auth_code::NO_PERMISSION,
            },
            ..AuthData::default()
        }
    };
    super::core::auth_done(zone, p, ad);
    st(zone).pending_auths -= 1;
}

/// Splits `:recipient:(sender)>text` (ASSS takes the first `)>`).
fn parse_remote(text: &[u8]) -> Option<(Vec<u8>, Vec<u8>, &[u8])> {
    let (recipient, rest) = delimcpy(text.get(1..)?, 32, b':');
    let rest = rest?;
    if rest.first() != Some(&b'(') {
        return None;
    }
    let end = rest.windows(2).skip(1).position(|w| w == b")>")? + 1;
    if end > 30 {
        return None;
    }
    Some((recipient, rest[1..end].to_vec(), &rest[end + 2..]))
}

fn process_zone_message(zone: &mut Zone, kind: u8, sound: u8, text: &[u8]) {
    if kind != 2 || text.is_empty() {
        tracing::warn!(target: "billing_ssc", "invalid chat packet");
        return;
    }
    if text.first() == Some(&b':') {
        let Some((recipient, sender, msg)) = parse_remote(text) else {
            return;
        };
        let sender = latin1(&sender);
        let msg = latin1(msg);
        if let Some(squad) = recipient.strip_prefix(b"#") {
            if squad.is_empty() {
                return;
            }
            let squad = latin1(squad);
            let to: Vec<PlayerId> = zone
                .players
                .iter()
                .filter(|p| p.squad.eq_ignore_ascii_case(&squad))
                .map(|p| p.id)
                .collect();
            send_remote_priv(zone, &to, sound, Some(&squad), &sender, &msg);
        } else {
            let r = latin1(&recipient);
            match zone.players.find_by_name(&r).map(|p| p.id) {
                Some(p) => send_remote_priv(zone, &[p], sound, None, &sender, &msg),
                None => {
                    tracing::debug!(target: "billing_ssc", "unknown destination for incoming remote priv: {r}");
                }
            }
        }
    } else {
        // a broadcast
        let to: Vec<PlayerId> = zone
            .players
            .iter()
            .filter(|p| p.status == PlayerStatus::Playing && p.arena.is_some())
            .map(|p| p.id)
            .collect();
        if let Some(c) = zone.iface::<dyn Chat>() {
            c.send_any_message(zone, &to, chat_type::ARENA, sound, -1, &latin1(text));
        }
    }
}

fn channel_message(zone: &mut Zone, p: PlayerId, channel: u8, text: &[u8]) {
    if let Some(c) = zone.iface::<dyn Chat>() {
        let t = format!("{channel}:{}", latin1(text));
        c.send_any_message(zone, &[p], chat_type::CHANNEL, 0, -1, &t);
    }
}

fn process_packet(zone: &mut Zone, data: &[u8]) {
    // any packet means the login worked
    if st(zone).state == LinkState::WaitLogin {
        logged_in(zone);
    }
    let pkt = match BillerToServer::decode(data) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "billing_ssc", kind = data.first().copied(), "bad packet from biller: {e}");
            return;
        }
    };
    match pkt {
        BillerToServer::UserLogin(l) => process_user_login(zone, data, l),
        BillerToServer::ZoneMessage {
            kind, sound, text, ..
        } => process_zone_message(zone, kind, sound, &text),
        BillerToServer::Permit { name, .. } => {
            // Another zone asked the biller to add this name to our permit list.
            // ASSS keeps no permit list and acts only on kind 2, so drop it.
            tracing::info!(target: "billing_ssc", name = %latin1(&name), "ignoring permit entry from biller");
        }
        BillerToServer::Kick {
            connection_id,
            reason,
        } => {
            if data.len() < 7 {
                return;
            }
            if let Some(p) = player_for(zone, connection_id) {
                tracing::info!(target: "billing_ssc", pid = p.0, reason, "player kicked out by user database server");
                super::core::kick(zone, p);
            }
        }
        BillerToServer::ArenaMessage {
            connection_id,
            text,
        } => {
            if text.is_empty() {
                return;
            }
            let Some(p) = player_for(zone, connection_id) else {
                return;
            };
            let t = latin1(&text);
            let msg = match t.find('|') {
                Some(i) if t.starts_with("$l$") => format!("(local) {}", &t[i + 1..]),
                Some(i) if t.starts_with("$s$") => format!("(staff) {}", &t[i + 1..]),
                _ => t,
            };
            message(zone, p, &msg);
        }
        BillerToServer::ChannelMessage {
            connection_id,
            channel,
            text,
        } => {
            if let Some(p) = player_for(zone, connection_id) {
                channel_message(zone, p, channel, &text);
            }
        }
        BillerToServer::MultiChannelChat { recipients, text } => {
            for (cid, channel) in recipients {
                if let Some(p) = player_for(zone, cid) {
                    channel_message(zone, p, channel, &text);
                }
            }
        }
        BillerToServer::ScoreReset {
            score_id,
            score_id_neg,
        } => {
            if score_id as i32 != (score_id_neg as i32).wrapping_neg() {
                tracing::warn!(target: "billing_ssc", "invalid scorereset packet");
                return;
            }
            tracing::info!(target: "billing_ssc", "billing server requested score reset");
            super::persist::end_public_interval(zone, Interval::Reset);
        }
        BillerToServer::UserPacket {
            connection_id,
            data: d,
        } => {
            if d.is_empty() {
                tracing::warn!(target: "billing_ssc", "invalid user packet");
            } else if connection_id == u32::MAX {
                tracing::warn!(target: "billing_ssc", "b2s user packet filtered (target all)");
            } else if let Some(p) = player_for(zone, connection_id) {
                // only the login text, so banned players see why
                if d[0] == s2c::kind::LOGIN_TEXT {
                    zone.send(p, &d, SendFlags::RELIABLE);
                } else {
                    tracing::warn!(target: "billing_ssc", pid = p.0, "b2s user packet filtered");
                }
            } else {
                tracing::warn!(target: "billing_ssc", pid = connection_id, "b2s user packet for unknown pid");
            }
        }
        BillerToServer::Identity(id) => {
            st(zone).identity = Some(cut(id, 256));
        }
        _ => {
            tracing::warn!(target: "billing_ssc", kind = data.first().copied(), "unsupported packet type");
        }
    }
}

fn logged_in(zone: &mut Zone) {
    announce(
        zone,
        "Notice: Connection to user database server restored. Log in again for full functionality",
    );
    send_s2b(zone, &ServerToBiller::ServerCapabilities { flags: 1 });
    let s = st(zone);
    s.state = LinkState::LoggedIn;
    s.identity = None;
    tracing::info!(target: "billing_ssc", "logged in to user database server");
}

fn got_connection(zone: &mut Zone) {
    let g = zone.cfg.global().clone();
    let port = u16::try_from(g.get_int("Listen", "Port", 5000)).unwrap_or(0);
    let pkt = ServerToBiller::ServerConnect {
        server_id: g.get_int("Billing", "ServerID", 0) as u32,
        group_id: g.get_int("Billing", "GroupID", 1) as u32,
        score_id: g.get_int("Billing", "ScoreID", 0) as u32,
        name: fixed(&to_latin1(g.get("Billing", "ServerName").unwrap_or(""))),
        port,
        password: fixed(&to_latin1(g.get("Billing", "Password").unwrap_or(""))),
    };
    send_s2b(zone, &pkt);
    let now = zone.now();
    let s = st(zone);
    s.state = LinkState::WaitLogin;
    s.last_event = now;
    tracing::info!(target: "billing_ssc", "connected to user database server, logging in");
}

fn poll_link(zone: &mut Zone) -> bool {
    let Some(mut link) = st(zone).link.take() else {
        return true;
    };
    let mut events = Vec::new();
    link.poll(&mut events);
    st(zone).link = Some(link);
    for ev in events {
        match ev {
            LinkEvent::Connected => got_connection(zone),
            LinkEvent::Packet(p) => {
                if st(zone).link.is_some() {
                    process_packet(zone, &p);
                }
            }
            LinkEvent::Disconnected => {
                if st(zone).link.take().is_some() {
                    let secs = st(zone).retry_ticks / 100;
                    tracing::info!(target: "billing_ssc", "lost connection to user database server (auto-retry in {secs} seconds)");
                    drop_connection(zone, LinkState::Retry);
                }
            }
        }
    }
    true
}

fn iterate(zone: &mut Zone) -> bool {
    let now = zone.now();
    match st(zone).state {
        LinkState::NoSocket => {
            let g = zone.cfg.global().clone();
            let Some(ip) = g.get("Billing", "IP").map(str::to_owned) else {
                tracing::warn!(target: "billing_ssc", "no Billing:IP set. user database disabled");
                drop_connection(zone, LinkState::Disabled);
                return true;
            };
            let port = u16::try_from(g.get_int("Billing", "Port", 1850)).unwrap_or(1850);
            let link = ip.trim().parse::<std::net::IpAddr>().ok().and_then(|a| {
                zone.iface::<dyn BillerConnector>()
                    .and_then(|c| c.connect(SocketAddr::new(a, port)))
            });
            let s = st(zone);
            s.last_event = now;
            match link {
                Some(l) => {
                    s.link = Some(l);
                    s.state = LinkState::Connecting;
                    tracing::info!(target: "billing_ssc", "connecting to user database server at {ip}:{port}");
                }
                None => {
                    s.state = LinkState::Retry;
                    tracing::warn!(target: "billing_ssc", "can't connect to user database server at {ip}:{port}");
                }
            }
        }
        LinkState::WaitLogin => {
            // only SSC answers a good login; assume one after a few seconds
            if now.since(st(zone).last_event) >= 500 {
                logged_in(zone);
            }
        }
        LinkState::LoggedIn => {
            if now.since(st(zone).last_event) >= 6000 {
                send_s2b(zone, &ServerToBiller::Ping);
                st(zone).last_event = now;
            }
            let s = st(zone);
            if now.since(s.interrupted_damp) >= 1000 {
                s.interrupted_auths /= 2;
                s.interrupted_damp = now;
            }
        }
        LinkState::Retry => {
            let s = st(zone);
            if now.since(s.last_event) > s.retry_ticks {
                s.state = LinkState::NoSocket;
            }
        }
        LinkState::Connecting | LinkState::LoginFailed | LinkState::Disabled => {}
    }
    true
}

struct BillingImpl;

impl Billing for BillingImpl {
    fn status(&self, zone: &Zone) -> BillingStatus {
        match link_state(zone) {
            LinkState::LoggedIn => BillingStatus::Up,
            LinkState::LoginFailed | LinkState::Disabled => BillingStatus::Disabled,
            _ => BillingStatus::Down,
        }
    }

    fn identity(&self, zone: &Zone) -> Option<Vec<u8>> {
        zone.res
            .get::<BillingState>()
            .and_then(|s| s.identity.clone())
    }
}

/// The `billing_ssc` module.
pub struct BillingSscModule;

impl Module for BillingSscModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let retry = zone.cfg.global().get_int("Billing", "RetryInterval", 180);
        zone.res.insert(BillingState {
            retry_ticks: u64::try_from(retry).unwrap_or(180) * 100,
            last_event: zone.now(),
            ..BillingState::default()
        });
        if zone.iface::<dyn BillerConnector>().is_none() {
            zone.reg
                .provide::<dyn BillerConnector>(me, Scope::Global, Arc::new(UdpConnector));
        }
        zone.on_packet(me, oss_proto::c2s::kind::REGISTRATION, on_demographics);
        zone.on::<PlayerAction>(me, Scope::Global, on_player_action);
        zone.on::<ChatMessage>(me, Scope::Global, on_chat);
        zone.on::<BannerSet>(me, Scope::Global, on_banner);
        zone.set_timer(me, 1, 1, poll_link);
        zone.set_timer(me, 10, 10, iterate);
        zone.add_command(
            me,
            "usage",
            None,
            Some(
                "Targets: player or none\nArgs: none\n\
                 Displays the usage information (current hours and minutes logged in, and\n\
                 total hours and minutes logged in), as well as the first login time, of\n\
                 the target player, or you if no target.\n",
            ),
            c_usage,
        );
        zone.add_command(
            me,
            "userid",
            None,
            Some(
                "Targets: player or none\nArgs: none\n\
                 Displays the user database id of the target player, or yours if no\n\
                 target.\n",
            ),
            c_userid,
        );
        zone.add_command(
            me,
            "userdbadm",
            None,
            Some(
                "Targets: none\nArgs: status|drop|connect\n\
                 The subcommand 'status' reports the status of the user database server\n\
                 connection. 'drop' disconnects the connection if it's up, and 'connect'\n\
                 reconnects after dropping or failed login.\n",
            ),
            c_userdbadm,
        );
        zone.commands.set_default(me, Arc::new(c_default));
        zone.reg
            .provide::<dyn Auth>(me, Scope::Global, Arc::new(BillingAuth));
        zone.reg
            .provide::<dyn Billing>(me, Scope::Global, Arc::new(BillingImpl));
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        drop_connection(zone, LinkState::Disabled);
        zone.res.remove::<BillingState>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_rewrite() {
        let local = Some(("zonechat,other", "LP"));
        let staff = Some(("staff", "SP"));
        assert_eq!(
            rewrite_chat_command("chat=ZoneChat,public/x,staff", local, staff),
            "?chat=$l$LP|ZoneChat,public/x,$s$SP|staff"
        );
        assert_eq!(rewrite_chat_command("chat=a,b", None, None), "?chat=a,b");
        assert!(find_chat("foo", "\tbar;FOO,baz"));
        assert!(!find_chat("fo", "foo"));
    }

    #[test]
    fn remote_parsing() {
        let (r, s, m) = parse_remote(b":bob:(funny)>name)>hi").expect("parse");
        assert_eq!(
            (r.as_slice(), s.as_slice(), m),
            (&b"bob"[..], &b"funny"[..], &b"name)>hi"[..])
        );
        assert!(parse_remote(b":bob:nope").is_none());
        assert!(parse_remote(b":bob").is_none());
        assert!(parse_remote(b":").is_none());
    }
}
