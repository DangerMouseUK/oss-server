//! `core`: the player lifecycle (ASSS `core.c` + the kick/find parts of
//! `player.c`).
//!
//! Players move through [`PlayerStatus`] states once per tick. Steps that
//! need another module's answer (authentication, persistent data) call out
//! through an interface and wait until that module reports back with
//! [`auth_done`] or [`player_sync_done`], which may happen on a later tick
//! (e.g. after a billing server replies via `ZoneHandle::run`).
//!
//! Events raised: [`NewPlayer`] when a player is allocated or freed, and
//! [`PlayerAction`] for connect, disconnect and arena entry/exit.

use std::net::SocketAddr;
use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s::{self, Login};
use oss_proto::s2c::LoginResponse;

use crate::arena::{ArenaId, ArenaStatus};
use crate::config::{latin1, to_latin1};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus, Ship};
use crate::registry::{Event, Owner};
use crate::zone::Zone;

/// ASSS authentication result codes (`AUTH_*`), as sent in the login response.
pub mod auth_code {
    /// Success.
    pub const OK: u8 = 0x00;
    /// Unknown name (client offers to register).
    pub const NEW_NAME: u8 = 0x01;
    /// Wrong password.
    pub const BAD_PASSWORD: u8 = 0x02;
    /// Arena full.
    pub const ARENA_FULL: u8 = 0x03;
    /// Locked out.
    pub const LOCKED_OUT: u8 = 0x04;
    /// No permission.
    pub const NO_PERMISSION: u8 = 0x05;
    /// Success, but spectator only.
    pub const SPEC_ONLY: u8 = 0x06;
    /// Too many points.
    pub const TOO_MANY_POINTS: u8 = 0x07;
    /// Connection too slow.
    pub const TOO_SLOW: u8 = 0x08;
    /// No permission (2).
    pub const NO_PERMISSION2: u8 = 0x09;
    /// Not accepting new connections.
    pub const NO_NEW_CONN: u8 = 0x0A;
    /// Bad player name.
    pub const BAD_NAME: u8 = 0x0B;
    /// Offensive player name.
    pub const OFFENSIVE_NAME: u8 = 0x0C;
    /// Success, but scores aren't recorded.
    pub const NO_SCORES: u8 = 0x0D;
    /// Server busy.
    pub const SERVER_BUSY: u8 = 0x0E;
    /// Usage too low.
    pub const TOO_LOW_USAGE: u8 = 0x0F;
    /// No name sent.
    pub const NO_NAME: u8 = 0x10;
    /// Too many demo players.
    pub const TOO_MANY_DEMO: u8 = 0x11;
    /// No demo players.
    pub const NO_DEMO: u8 = 0x12;
    /// Custom text (Continuum only).
    pub const CUSTOM_TEXT: u8 = 0x13;

    /// Codes that let the player in.
    pub fn is_ok(code: u8) -> bool {
        matches!(code, OK | SPEC_ONLY | NO_SCORES)
    }
}

/// SubSpace 1.34 / VIE client version.
pub const VIE_CLIENT_VERSION: u16 = 134;
/// Server version reported in the login response.
pub const SERVER_VERSION: u32 = 134;
/// Exe checksum reported to VIE clients: the CRC-32 of the 630,784-byte
/// `SUBSPACE.EXE` 1.34, which is what the original `subgame` sends. The client
/// checks this one first and offers an update when it disagrees.
pub const VIE_EXE_CHECKSUM: u32 = 0xF142_9CE8;
/// Code checksum reported to VIE clients. **Zero on purpose; do not restore
/// ASSS's value.**
///
/// In the VIE protocol zero means "not applicable". Decrypting the original
/// `subgame2.exe` on the wire shows it sending `codechecksum = 0` beside the
/// real `exechecksum` above, while ASSS `core.c:793` sends `0x281C_C948`; a
/// genuine 1.34 client answers that value with `SEC_LoginChecksumMismatch`
/// (`0x17`) and exits. Only staff ever got in, because the staff path below
/// overwrites both fields with `0xFFFF_FFFF`, which the client skips.
///
/// Measured 2026-11-13: with this at zero and nothing else changed, a player
/// with no capabilities logged in and entered an arena from the 1998 client.
/// The security response carries the same convention the other way round --
/// see `modules::security`.
pub const VIE_CODE_CHECKSUM: u32 = 0;

/// What a player sent to log in, after ASSS's sanitising.
#[derive(Debug, Clone)]
pub struct LoginRequest {
    /// Name (sanitised, at most 19 bytes).
    pub name: String,
    /// Password (at most 31 bytes).
    pub password: String,
    /// The client asked to create a new account.
    pub new_user: bool,
    /// Client version.
    pub client_version: u16,
    /// Machine id.
    pub machine_id: u32,
    /// Permission id.
    pub permission_id: u32,
    /// The decoded login packet.
    pub packet: Login,
}

/// An authentication result (ASSS `AuthData`).
#[derive(Debug, Clone, Default)]
pub struct AuthData {
    /// [`auth_code`].
    pub code: u8,
    /// Ask the client for registration data.
    pub demo_data: bool,
    /// Password verified (enables staff groups by name).
    pub authenticated: bool,
    /// Name to assign.
    pub name: String,
    /// Name shown to clients (at most 20 bytes).
    pub send_name: String,
    /// Squad.
    pub squad: String,
    /// Text for [`auth_code::CUSTOM_TEXT`].
    pub custom_text: String,
}

impl AuthData {
    /// A successful result for `name`, not authenticated.
    pub fn ok(name: &str) -> Self {
        Self {
            code: auth_code::OK,
            name: name.to_owned(),
            send_name: name.to_owned(),
            ..Self::default()
        }
    }
}

/// Authenticates players (ASSS `Iauth`). The implementation must eventually
/// call [`auth_done`] for the player, now or later.
pub trait Auth: Send + Sync {
    /// Starts authenticating `player`.
    fn authenticate(&self, zone: &mut Zone, player: PlayerId, login: &LoginRequest);
}

/// Checked before authentication, whichever auth module is loaded (bans).
/// Returning a result refuses the login with it.
pub trait LoginGate: Send + Sync {
    /// `Some(result)` to refuse `player`'s login.
    fn check(&self, zone: &mut Zone, player: PlayerId, login: &LoginRequest) -> Option<AuthData>;
}

/// Loads and saves persistent player data (ASSS `Ipersist`). Each call must
/// eventually be answered with [`player_sync_done`]. Without a provider the
/// core continues immediately.
pub trait Persist: Send + Sync {
    /// Load global (`arena == None`) or per-arena data.
    fn get_player(&self, zone: &mut Zone, player: PlayerId, arena: Option<ArenaId>);
    /// Save global or per-arena data.
    fn put_player(&self, zone: &mut Zone, player: PlayerId, arena: Option<ArenaId>);
}

/// Picks a player's initial ship and frequency (ASSS `Ifreqman::InitialFreq`).
pub trait FreqManager: Send + Sync {
    /// Adjusts `ship`/`freq` for a player entering `arena`.
    fn initial_freq(&self, zone: &mut Zone, player: PlayerId, ship: &mut Ship, freq: &mut u16);
    /// Adjusts a requested ship change (`ship` = requested, `freq` = current).
    fn ship_change(&self, _zone: &mut Zone, _player: PlayerId, _ship: &mut Ship, _freq: &mut u16) {}
    /// Adjusts a requested frequency change (`freq` = requested, `ship` = current).
    fn freq_change(&self, _zone: &mut Zone, _player: PlayerId, _ship: &mut Ship, _freq: &mut u16) {}
}

/// What happened to a player (ASSS `PA_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Logged in (after authentication and global data).
    Connect,
    /// Leaving the zone.
    Disconnect,
    /// About to enter an arena (ship/freq not assigned yet).
    PreEnterArena,
    /// Entered an arena.
    EnterArena,
    /// Leaving an arena.
    LeaveArena,
}

/// ASSS `CB_PLAYERACTION`.
#[derive(Debug, Clone, Copy)]
pub struct PlayerAction {
    /// The player.
    pub player: PlayerId,
    /// What happened.
    pub action: Action,
    /// The arena, for arena actions.
    pub arena: Option<ArenaId>,
}

impl Event for PlayerAction {
    fn arena(&self) -> Option<ArenaId> {
        self.arena
    }
}

/// ASSS `CB_NEWPLAYER`: a player slot was allocated (`created`) or is about
/// to be freed.
#[derive(Debug, Clone, Copy)]
pub struct NewPlayer {
    /// The player.
    pub player: PlayerId,
    /// True when allocated, false when freed.
    pub created: bool,
}

impl Event for NewPlayer {}

#[derive(Default)]
struct CoreData {
    login: Option<LoginRequest>,
    auth: Option<AuthData>,
    has_done_gsync: bool,
    has_done_gcallbacks: bool,
    has_done_async: bool,
    replaced_by: Option<PlayerId>,
    leave_arena_when_done_waiting: bool,
}

fn data(zone: &mut Zone, p: PlayerId) -> Option<&mut CoreData> {
    zone.players
        .get_mut(p)
        .map(|p| p.ext.get_or_default::<CoreData>())
}

fn set_status(zone: &mut Zone, p: PlayerId, s: PlayerStatus) {
    if let Some(pl) = zone.players.get_mut(p) {
        pl.status = s;
    }
}

fn status(zone: &Zone, p: PlayerId) -> Option<PlayerStatus> {
    zone.players.get(p).map(|p| p.status)
}

/// Continuum checksums from the zone directory (ASSS reads
/// `clients/continuum.exe` and `scrty`); `u32::MAX` when absent.
#[derive(Debug, Clone, Copy)]
struct ContChecksums {
    exe: u32,
    code: u32,
}

/// Allocates a player for a new connection, in [`PlayerStatus::Connected`].
/// Transports call this once their handshake is done.
pub fn new_connection(
    zone: &mut Zone,
    kind: ClientKind,
    addr: Option<SocketAddr>,
) -> Option<PlayerId> {
    let id = zone.players.alloc(kind, zone.now())?;
    if let Some(p) = zone.players.get_mut(id) {
        p.addr = addr;
        p.status = PlayerStatus::Connected;
    }
    zone.fire(&NewPlayer {
        player: id,
        created: true,
    });
    Some(id)
}

/// ASSS `KickPlayer`: takes the player out of their arena and then out of the
/// zone, running every leave callback on the way.
pub fn kick(zone: &mut Zone, player: PlayerId) {
    if zone.players.get(player).and_then(|p| p.arena).is_some() {
        super::arenaman::leave_arena(zone, player);
    }
    if let Some(p) = zone.players.get_mut(player) {
        p.when_logged_in = Some(PlayerStatus::LeavingZone);
    }
}

/// Reports an authentication result (ASSS `AuthDone`).
pub fn auth_done(zone: &mut Zone, player: PlayerId, auth: AuthData) {
    let Some(st) = status(zone, player) else {
        return;
    };
    if st != PlayerStatus::WaitAuth {
        tracing::warn!(target: "core", pid = player.0, ?st, "auth_done from wrong state");
        return;
    }
    if let Some(p) = zone.players.get_mut(player) {
        p.authenticated = auth.authenticated;
    }
    let ok = auth_code::is_ok(auth.code);
    let name = auth.name.clone();
    let squad = auth.squad.clone();
    // ASSS copies `sendname` into the player-data packet and `name` into the
    // player; an auth module that leaves `sendname` empty means the two are
    // the same
    let send_name = if auth.send_name.is_empty() {
        name.clone()
    } else {
        auth.send_name.clone()
    };
    if let Some(d) = data(zone, player) {
        d.auth = Some(auth);
    }
    if ok {
        let old = zone
            .players
            .find_by_name(&name)
            .map(|p| p.id)
            .filter(|&o| o != player);
        if let Some(p) = zone.players.get_mut(player) {
            p.name = truncate(&name, 20);
            p.send_name = truncate(&send_name, 20);
            p.squad = truncate(&squad, 20);
        }
        match old {
            Some(old) => {
                tracing::debug!(target: "core", name = %name, new = player.0, old = old.0, "player already on, kicking old one");
                if let Some(d) = data(zone, old) {
                    d.replaced_by = Some(player);
                }
                kick(zone, old);
            }
            None => set_status(zone, player, PlayerStatus::NeedGlobalSync),
        }
    } else {
        send_login_response(zone, player);
        set_status(zone, player, PlayerStatus::Connected);
    }
}

/// Reports that a [`Persist`] load or save finished (ASSS `player_sync_done`).
pub fn player_sync_done(zone: &mut Zone, player: PlayerId) {
    let Some(st) = status(zone, player) else {
        return;
    };
    let next = match st {
        PlayerStatus::WaitArenaSync1 => {
            let leave = data(zone, player).is_some_and(|d| d.leave_arena_when_done_waiting);
            if leave {
                if let Some(d) = data(zone, player) {
                    d.leave_arena_when_done_waiting = false;
                }
                PlayerStatus::DoArenaSync2
            } else {
                PlayerStatus::ArenaRespAndCbs
            }
        }
        PlayerStatus::WaitArenaSync2 => PlayerStatus::LoggedIn,
        PlayerStatus::WaitGlobalSync1 => PlayerStatus::DoGlobalCallbacks,
        PlayerStatus::WaitGlobalSync2 => {
            let replaced = data(zone, player).and_then(|d| d.replaced_by.take());
            if let Some(r) = replaced {
                if status(zone, r) == Some(PlayerStatus::WaitAuth) {
                    set_status(zone, r, PlayerStatus::NeedGlobalSync);
                } else {
                    tracing::warn!(target: "core", old = player.0, new = r.0, "unexpected status when replacing player");
                }
            }
            PlayerStatus::TimeWait
        }
        _ => {
            tracing::warn!(target: "core", pid = player.0, ?st, "player_sync_done from wrong state");
            return;
        }
    };
    set_status(zone, player, next);
}

/// Marks a player that's still entering as "leave once loaded" (used by the
/// arena manager while persistent data is loading).
pub(crate) fn leave_arena_when_done_waiting(zone: &mut Zone, player: PlayerId) {
    if let Some(d) = data(zone, player) {
        d.leave_arena_when_done_waiting = true;
    }
}

fn truncate(s: &str, max_bytes: usize) -> String {
    s.chars().take(max_bytes).collect()
}

/// ASSS name sanitising: printable ASCII except `:`, runs of spaces
/// collapsed, leading spaces removed.
fn clean_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut last = b' ';
    for &c in raw {
        if (32..=126).contains(&c) && c != b':' {
            if c == b' ' && last == b' ' {
                continue;
            }
            out.push(c);
            last = c;
        }
    }
    out
}

fn cstr(b: &[u8], max: usize) -> &[u8] {
    let b = &b[..b.len().min(max)];
    match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    }
}

fn fail_login(zone: &mut Zone, player: PlayerId, code: u8, text: Option<&str>, why: &str) {
    // Continuum and chat clients can show a custom reason
    let is_cont = zone
        .players
        .get(player)
        .is_some_and(|p| matches!(p.kind, ClientKind::Continuum | ClientKind::Chat));
    let mut auth = AuthData::default();
    match text {
        Some(t) if is_cont => {
            auth.code = auth_code::CUSTOM_TEXT;
            auth.custom_text = t.to_owned();
        }
        _ => auth.code = code,
    }
    set_status(zone, player, PlayerStatus::WaitAuth);
    auth_done(zone, player, auth);
    tracing::debug!(target: "core", pid = player.0, "login denied: {why}");
}

fn on_login(zone: &mut Zone, player: PlayerId, pkt: &[u8]) {
    let Some(p) = zone.players.get(player) else {
        return;
    };
    let (kind, st) = (p.kind, p.status);
    let want = match kind {
        // chatnet builds a VIE-sized login from its LOGIN line
        ClientKind::Vie | ClientKind::Chat => Login::VIE_LEN,
        ClientKind::Continuum => Login::CONT_LEN,
        _ => {
            tracing::warn!(target: "core", pid = player.0, ?kind, "login packet from wrong client type");
            return;
        }
    };
    if pkt.len() != want {
        tracing::warn!(target: "core", pid = player.0, len = pkt.len(), "bad login packet length");
        return;
    }
    if st != PlayerStatus::Connected {
        tracing::warn!(target: "core", pid = player.0, ?st, "login from wrong stage");
        return;
    }
    let Ok(login) = Login::decode(pkt) else {
        tracing::warn!(target: "core", pid = player.0, "undecodable login packet");
        return;
    };
    if kind == ClientKind::Vie && login.client_version != VIE_CLIENT_VERSION {
        fail_login(
            zone,
            player,
            auth_code::LOCKED_OUT,
            None,
            "bad VIE client version",
        );
        return;
    }
    let raw_name = cstr(login.name.as_bytes(), 19);
    if raw_name.is_empty() {
        fail_login(
            zone,
            player,
            auth_code::NO_NAME,
            Some("Your player name is blank"),
            "name is blank",
        );
        return;
    }
    let name = clean_name(raw_name);
    if name.is_empty() {
        fail_login(
            zone,
            player,
            auth_code::BAD_NAME,
            Some("Your player name contains no valid characters"),
            "all invalid chars",
        );
        return;
    }
    // ASSS `core.c` refuses the login here rather than repairing the name,
    // and this is what makes its `auth_prefix` module unreachable: that
    // module exists to take a leading `+ - > @ # $` off a staff member's
    // name before authentication and put it back on the name other players
    // see, but no such name ever reaches an auth module. A game client is
    // refused right here, and a chat client's leading punctuation has
    // already been rewritten to `_` on its own login path (`chatnet.rs`),
    // which is also what ASSS does. 1.4.4 ships `auth_prefix` commented out
    // in `modules.conf`; it is not ported here, and porting it would mean
    // relaxing this refusal, which is a zone-visible change to who may log
    // in rather than a parity fix.
    if !name[0].is_ascii_alphanumeric() {
        fail_login(
            zone,
            player,
            auth_code::BAD_NAME,
            Some("Your player name must start with a letter or number"),
            "name doesn't start with alphanumeric",
        );
        return;
    }
    let req = LoginRequest {
        name: latin1(&name),
        password: latin1(cstr(login.password.as_bytes(), 31)),
        new_user: login.flags != 0,
        client_version: login.client_version,
        machine_id: login.machine_id,
        permission_id: login.permission_id,
        packet: login,
    };
    if let Some(p) = zone.players.get_mut(player) {
        p.machine_id = req.machine_id;
        p.permission_id = req.permission_id;
        p.client_version = req.client_version;
        p.client_name = match kind {
            ClientKind::Continuum => format!("<continuum, v. {}>", req.client_version),
            // chatnet names chat clients itself
            ClientKind::Chat => p.client_name.clone(),
            _ => format!("<ss/vie client, v. {}>", req.client_version),
        };
        p.status = PlayerStatus::NeedAuth;
    }
    tracing::debug!(target: "core", pid = player.0, name = %req.name, "login request");
    if let Some(d) = data(zone, player) {
        d.login = Some(req);
    }
}

/// Sends the login response for the player's stored auth result.
fn send_login_response(zone: &mut Zone, player: PlayerId) {
    let Some(auth) = data(zone, player).and_then(|d| d.auth.take()) else {
        tracing::error!(target: "core", pid = player.0, "missing auth data");
        kick(zone, player);
        return;
    };
    let Some(kind) = zone.players.get(player).map(|p| p.kind) else {
        return;
    };
    if kind == ClientKind::Chat {
        let line = if auth_code::is_ok(auth.code) {
            let name = zone
                .players
                .get(player)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            format!("LOGINOK:{name}")
        } else if auth.code == auth_code::CUSTOM_TEXT {
            format!("LOGINBAD:{}", auth.custom_text)
        } else {
            format!("LOGINBAD:{}", auth_code_message(auth.code))
        };
        super::chatnet::send_line(zone, player, &line);
        return;
    }
    if !matches!(kind, ClientKind::Vie | ClientKind::Continuum) {
        return;
    }
    let cont = zone
        .res
        .get::<ContChecksums>()
        .copied()
        .unwrap_or(ContChecksums {
            exe: u32::MAX,
            code: u32::MAX,
        });
    let news = zone.res.get::<NewsChecksum>().map_or(0, |n| n.0);
    let mut lr = LoginResponse {
        code: auth.code,
        server_version: SERVER_VERSION,
        is_vip: 0,
        unused1: [0; 3],
        exe_checksum: VIE_EXE_CHECKSUM,
        unused2: [0; 5],
        demo_data: u8::from(auth.demo_data),
        code_checksum: VIE_CODE_CHECKSUM,
        news_checksum: news,
        unused3: [0; 8],
    };
    let mut buf = Vec::with_capacity(64);
    if kind == ClientKind::Continuum {
        // S2C 0x34: Continuum version (40) and exe checksum
        buf.push(oss_proto::s2c::kind::CONT_VERSION);
        buf.extend_from_slice(&40u16.to_le_bytes());
        buf.extend_from_slice(&cont.exe.to_le_bytes());
        zone.send(player, &buf, SendFlags::RELIABLE);
        lr.exe_checksum = cont.exe;
        lr.code_checksum = cont.code;
    }
    let caps = zone.iface::<dyn super::capman::Capabilities>();
    if caps.is_some_and(|c| c.has_capability(zone, player, super::capman::CAP_SEE_PRIV_FREQ)) {
        // makes the client treat the player as staff
        lr.exe_checksum = u32::MAX;
        lr.code_checksum = u32::MAX;
    }
    if lr.code == auth_code::CUSTOM_TEXT {
        if kind == ClientKind::Continuum {
            buf.clear();
            buf.push(oss_proto::s2c::kind::LOGIN_TEXT);
            let mut t = to_latin1(&auth.custom_text);
            t.truncate(254);
            buf.extend_from_slice(&t);
            buf.push(0);
            zone.send(player, &buf, SendFlags::RELIABLE);
        } else {
            lr.code = auth_code::LOCKED_OUT;
        }
    }
    buf.clear();
    lr.encode(&mut buf);
    zone.send(player, &buf, SendFlags::RELIABLE);
}

/// ASSS `get_auth_code_msg`: login results as chat clients see them.
pub fn auth_code_message(code: u8) -> &'static str {
    match code {
        auth_code::OK => "ok",
        auth_code::NEW_NAME => "new user",
        auth_code::BAD_PASSWORD => "incorrect password",
        auth_code::ARENA_FULL => "arena full",
        auth_code::LOCKED_OUT => "you have been locked out",
        auth_code::NO_PERMISSION => "no permission",
        auth_code::SPEC_ONLY => "you can spec only",
        auth_code::TOO_MANY_POINTS => "you have too many points",
        auth_code::TOO_SLOW => "too slow (?)",
        auth_code::NO_PERMISSION2 => "no permission (2)",
        auth_code::NO_NEW_CONN => "the server is not accepting new connections",
        auth_code::BAD_NAME => "bad player name",
        auth_code::OFFENSIVE_NAME => "offensive player name",
        auth_code::NO_SCORES => "the server is not recordng scores",
        auth_code::SERVER_BUSY => "the server is busy",
        auth_code::TOO_LOW_USAGE => "too low usage",
        auth_code::NO_NAME => "no name sent",
        auth_code::TOO_MANY_DEMO => "too many demo players",
        auth_code::NO_DEMO => "no demo players allowed",
        _ => "???",
    }
}

/// CRC-32 of the news file, set by the map/news module (0 = no news).
#[derive(Debug, Clone, Copy, Default)]
pub struct NewsChecksum(pub u32);

/// The default authenticator used when no auth module is loaded: lets
/// everyone in, unauthenticated (ASSS `DefaultAuth`).
pub struct DefaultAuth;

impl Auth for DefaultAuth {
    fn authenticate(&self, zone: &mut Zone, player: PlayerId, login: &LoginRequest) {
        auth_done(zone, player, AuthData::ok(&login.name));
    }
}

fn process_player_states(zone: &mut Zone) {
    let mut actions: Vec<(PlayerId, PlayerStatus)> = Vec::new();
    for p in zone.players.iter_mut() {
        let old = p.status;
        let next = match old {
            PlayerStatus::Connected | PlayerStatus::LoggedIn => {
                p.arena = None;
                if let Some(when) = p.when_logged_in.take() {
                    p.status = when;
                }
                continue;
            }
            PlayerStatus::NeedAuth => PlayerStatus::WaitAuth,
            PlayerStatus::NeedGlobalSync => PlayerStatus::WaitGlobalSync1,
            PlayerStatus::DoGlobalCallbacks => PlayerStatus::SendLoginResponse,
            PlayerStatus::SendLoginResponse => PlayerStatus::LoggedIn,
            PlayerStatus::DoFreqAndArenaSync => PlayerStatus::WaitArenaSync1,
            PlayerStatus::ArenaRespAndCbs => PlayerStatus::Playing,
            PlayerStatus::LeavingArena => PlayerStatus::DoArenaSync2,
            PlayerStatus::DoArenaSync2 => PlayerStatus::WaitArenaSync2,
            PlayerStatus::LeavingZone => PlayerStatus::WaitGlobalSync2,
            _ => continue,
        };
        p.status = next;
        actions.push((p.id, old));
    }
    // the arena-ready transition from LoggedIn is checked separately because
    // it needs the arena table
    let ready: Vec<(PlayerId, ArenaId)> = zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::LoggedIn || p.status == PlayerStatus::Connected)
        .filter_map(|p| p.new_arena.map(|a| (p.id, a)))
        .filter(|&(_, a)| {
            zone.arenas
                .get(a)
                .is_some_and(|a| a.status == ArenaStatus::Running)
        })
        .collect();
    for (pid, a) in ready {
        if let Some(p) = zone.players.get_mut(pid)
            && p.status == PlayerStatus::LoggedIn
        {
            p.arena = Some(a);
            p.new_arena = None;
            p.status = PlayerStatus::WaitArenaSync1;
            actions.push((pid, PlayerStatus::DoFreqAndArenaSync));
        }
    }

    let persist = zone.iface::<dyn Persist>();
    for (pid, old) in actions {
        match old {
            PlayerStatus::NeedAuth => {
                let login = data(zone, pid).and_then(|d| d.login.take());
                let auth: Arc<dyn Auth> = zone
                    .iface::<dyn Auth>()
                    .unwrap_or_else(|| Arc::new(DefaultAuth));
                let refused = match (&login, zone.iface::<dyn LoginGate>()) {
                    (Some(l), Some(g)) => g.check(zone, pid, l),
                    _ => None,
                };
                match login {
                    Some(_) if refused.is_some() => {
                        if let Some(ad) = refused {
                            auth_done(zone, pid, ad);
                        }
                    }
                    Some(l) => auth.authenticate(zone, pid, &l),
                    None => {
                        tracing::warn!(target: "core", pid = pid.0, "can't authenticate player");
                        kick(zone, pid);
                    }
                }
            }
            PlayerStatus::NeedGlobalSync => {
                if let Some(d) = data(zone, pid) {
                    d.has_done_gsync = true;
                }
                match &persist {
                    Some(ps) => ps.get_player(zone, pid, None),
                    None => player_sync_done(zone, pid),
                }
            }
            PlayerStatus::DoGlobalCallbacks => {
                if let Some(d) = data(zone, pid) {
                    d.has_done_gcallbacks = true;
                }
                zone.fire(&PlayerAction {
                    player: pid,
                    action: Action::Connect,
                    arena: None,
                });
            }
            PlayerStatus::SendLoginResponse => {
                send_login_response(zone, pid);
                let name = zone
                    .players
                    .get(pid)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                tracing::info!(target: "core", pid = pid.0, name = %name, "player logged in");
            }
            PlayerStatus::DoFreqAndArenaSync => {
                let Some(arena) = zone.players.get(pid).and_then(|p| p.arena) else {
                    continue;
                };
                let requested = zone.players.get(pid).map_or(Ship::SPEC, |p| p.ship);
                zone.fire(&PlayerAction {
                    player: pid,
                    action: Action::PreEnterArena,
                    arena: Some(arena),
                });
                let mut ship = requested;
                let mut freq = 0u16;
                if let Some(fm) = zone.iface_in::<dyn FreqManager>(arena) {
                    fm.initial_freq(zone, pid, &mut ship, &mut freq);
                } else if ship.is_spec() {
                    freq = zone
                        .arenas
                        .get(arena)
                        .map_or(8025, super::arenaman::spec_freq);
                }
                if let Some(p) = zone.players.get_mut(pid) {
                    p.ship = ship;
                    p.freq = freq;
                }
                if let Some(d) = data(zone, pid) {
                    d.has_done_async = true;
                }
                match &persist {
                    Some(ps) => ps.get_player(zone, pid, Some(arena)),
                    None => player_sync_done(zone, pid),
                }
            }
            PlayerStatus::ArenaRespAndCbs => {
                let arena = zone.players.get(pid).and_then(|p| p.arena);
                super::arenaman::send_arena_response(zone, pid);
                zone.fire(&PlayerAction {
                    player: pid,
                    action: Action::EnterArena,
                    arena,
                });
            }
            PlayerStatus::LeavingArena => {
                let arena = zone.players.get(pid).and_then(|p| p.arena);
                zone.fire(&PlayerAction {
                    player: pid,
                    action: Action::LeaveArena,
                    arena,
                });
            }
            PlayerStatus::DoArenaSync2 => {
                let arena = zone.players.get(pid).and_then(|p| p.arena);
                let did = data(zone, pid).is_some_and(|d| std::mem::take(&mut d.has_done_async));
                match (&persist, did) {
                    (Some(ps), true) => ps.put_player(zone, pid, arena),
                    _ => player_sync_done(zone, pid),
                }
            }
            PlayerStatus::LeavingZone => {
                let (gcb, gsync) = data(zone, pid)
                    .map(|d| (d.has_done_gcallbacks, std::mem::take(&mut d.has_done_gsync)))
                    .unwrap_or_default();
                if gcb {
                    zone.fire(&PlayerAction {
                        player: pid,
                        action: Action::Disconnect,
                        arena: None,
                    });
                }
                match (&persist, gsync) {
                    (Some(ps), true) => ps.put_player(zone, pid, None),
                    _ => player_sync_done(zone, pid),
                }
            }
            _ => {}
        }
    }

    // players that finished leaving: drop the connection and free the slot
    let gone: Vec<PlayerId> = zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::TimeWait)
        .map(|p| p.id)
        .collect();
    for pid in gone {
        free_player(zone, pid);
    }
}

/// Drops the connection and frees the player slot.
fn free_player(zone: &mut Zone, pid: PlayerId) {
    zone.disconnect(pid, "left zone");
    zone.fire(&NewPlayer {
        player: pid,
        created: false,
    });
    if let Some(p) = zone.players.remove(pid) {
        tracing::debug!(target: "core", pid = pid.0, name = %p.name, "player freed");
    }
}

/// The `core` module.
pub struct CoreModule;

impl Module for CoreModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let src = zone.cfg.source();
        let exe = src
            .read(std::path::Path::new("clients/continuum.exe"))
            .map_or(u32::MAX, |b| oss_proto::checksum::crc32(&b));
        let code = src
            .read(std::path::Path::new("scrty"))
            .and_then(|b| {
                b.get(4..8)
                    .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            })
            .unwrap_or(u32::MAX);
        zone.res.insert(ContChecksums { exe, code });
        zone.on_packet(me, c2s::kind::LOGIN, on_login);
        zone.on_packet(me, c2s::kind::CONT_LOGIN, on_login);
        zone.set_timer(me, 1, 1, |z| {
            process_player_states(z);
            true
        });
        // ASSS SendKeepalive: every 5 s, S2C 0x27 to everyone playing, so idle
        // clients keep seeing traffic
        zone.set_timer(me, 500, 500, |z| {
            let ids: Vec<PlayerId> = z
                .players
                .iter()
                .filter(|p| p.status == PlayerStatus::Playing)
                .map(|p| p.id)
                .collect();
            z.send_many(
                ids,
                &[oss_proto::s2c::kind::KEEP_ALIVE],
                SendFlags::UNRELIABLE,
            );
            true
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_like_asss() {
        assert_eq!(
            clean_name(b"  Bob   the:Builder\x01"),
            b"Bob theBuilder".to_vec()
        );
        assert_eq!(clean_name(b"trailing "), b"trailing ".to_vec());
        assert_eq!(clean_name(b"\x80\x90"), Vec::<u8>::new());
        assert_eq!(cstr(b"abc\0def", 19), b"abc");
        assert_eq!(cstr(&[b'x'; 30], 19).len(), 19);
    }
}
