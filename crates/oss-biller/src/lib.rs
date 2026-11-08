//! A local user database server ("biller") for Open SubSpace zones.
//!
//! Zones connect with ASSS's `billing_ssc` protocol (SubSpace transport,
//! VIE encryption) as they would to SSC. Accounts, play time, banners, scores
//! (per score realm), `?chat` channel lists and squads live in SQLite.
//!
//! What it does for zones:
//! - logins: checks passwords; unknown names get "new user" (the client then
//!   asks to register) unless registration is automatic or closed;
//! - chat channels: `?chat=a,b,c` sets up to ten channels, `;n;text` talks on
//!   one, across every connected zone; `?chat` lists who is on them;
//! - remote private messages (`:name:text`) and squad messages (`:#squad:text`)
//!   between zones;
//! - squads: `?squadcreate=name:password`, `?squadjoin=name:password`,
//!   `?squadleave`, `?squadgrant=player`, `?squadkick=player`,
//!   `?squadpassword=password`, `?squaddissolve`, `?squadlist[=squad]`,
//!   `?squad[=player]` (changes show at the next login, as with SSC);
//! - `?find player`, `?password=new`.
//!
//! It is meant for private and test zones on a LAN or localhost. The engine
//! ([`Biller`]) does no I/O itself; [`run`] drives it on a UDP socket.

pub mod db;

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oss_net::{Config, Event, Server, VieAcceptor};
use oss_proto::billing::{BillerLogin, BillerToServer, PlayerScore, ServerToBiller, login_result};
use oss_proto::wire::{FixedStr, Raw};

pub use db::Store;

/// What to do with names that have no account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Registration {
    /// Answer "new user" so the client asks the player to register.
    #[default]
    Ask,
    /// Create the account at once.
    Auto,
    /// Refuse new accounts.
    Closed,
}

/// Biller settings.
#[derive(Debug, Clone)]
pub struct BillerConfig {
    /// Password zones must send in their connect packet (`Billing:Password`).
    pub password: Option<String>,
    /// New-account policy.
    pub registration: Registration,
    /// Identity data sent to zones after they connect.
    pub identity: Vec<u8>,
}

impl Default for BillerConfig {
    fn default() -> Self {
        Self {
            password: None,
            registration: Registration::Ask,
            identity: b"oss-biller".to_vec(),
        }
    }
}

/// Most `?chat` channels a player can be on.
pub const MAX_CHANNELS: usize = 10;

#[derive(Debug, Clone)]
struct Online {
    user_id: u32,
    name: String,
    squad: String,
    chats: Vec<String>,
    since: u64,
}

#[derive(Debug, Default)]
struct ZoneConn {
    logged_in: bool,
    server_id: u32,
    score_id: u32,
    name: String,
    multicast: bool,
    players: HashMap<u32, Online>,
}

/// The biller engine.
pub struct Biller {
    store: Store,
    cfg: BillerConfig,
    server: Server<SocketAddr>,
    zones: HashMap<SocketAddr, ZoneConn>,
    start: Instant,
    out: Vec<(SocketAddr, Vec<u8>)>,
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

fn to_latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

fn fixed<const N: usize>(s: &str) -> FixedStr<N> {
    let b = to_latin1(s);
    FixedStr::new(&b[..b.len().min(N - 1)])
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A channel's key: local chats (`$l$...|name`, from the zone's
/// `Billing:LocalChats`) are kept per zone.
fn channel_key(zone: SocketAddr, chat: &str) -> String {
    let c = chat.to_ascii_lowercase();
    if c.starts_with("$l$") {
        format!("{zone}/{c}")
    } else {
        c
    }
}

/// A channel name without the zone's `$l$`/`$s$` marker.
fn channel_display(chat: &str) -> &str {
    if (chat.starts_with("$l$") || chat.starts_with("$s$"))
        && let Some(i) = chat.find('|')
    {
        return &chat[i + 1..];
    }
    chat
}

fn split_command(line: &str) -> (String, &str) {
    let end = line.find([' ', '=']).unwrap_or(line.len());
    (
        line[..end].to_ascii_lowercase(),
        line[end..].trim_start_matches([' ', '=']),
    )
}

fn score_of(s: &db::Score) -> PlayerScore {
    PlayerScore {
        kills: s.kills,
        deaths: s.deaths,
        flags: s.flags,
        points: s.points,
        flag_points: s.flag_points,
    }
}

fn db_score(s: &PlayerScore) -> db::Score {
    db::Score {
        kills: s.kills,
        deaths: s.deaths,
        flags: s.flags,
        points: s.points,
        flag_points: s.flag_points,
    }
}

impl Biller {
    /// A biller over `store`.
    pub fn new(store: Store, cfg: BillerConfig) -> Self {
        let mut server = Server::new(Config::server());
        server.add_acceptor(Box::new(VieAcceptor {
            accept_continuum: false,
        }));
        Self {
            store,
            cfg,
            server,
            zones: HashMap::new(),
            start: Instant::now(),
            out: Vec::new(),
        }
    }

    /// The database.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Number of zones connected and logged in.
    pub fn zones(&self) -> usize {
        self.zones.values().filter(|z| z.logged_in).count()
    }

    /// Number of players online in all zones.
    pub fn players_online(&self) -> usize {
        self.zones.values().map(|z| z.players.len()).sum()
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Feeds a datagram from `from`.
    pub fn handle_datagram(&mut self, from: SocketAddr, data: &[u8]) {
        let now = self.now();
        self.server.handle_datagram(now, from, data);
        self.events();
    }

    /// Timers: retransmission, keep-alives and timeouts.
    pub fn tick(&mut self) {
        let now = self.now();
        self.server.tick(now);
        self.events();
    }

    /// Next datagram to send.
    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        if self.out.is_empty() {
            let now = self.now();
            let addrs: Vec<SocketAddr> = self.server.addresses().copied().collect();
            for a in addrs {
                if let Some(c) = self.server.connection_mut(&a) {
                    c.flush(now);
                    while let Some(d) = c.poll_transmit() {
                        self.out.push((a, d));
                    }
                }
            }
            while let Some(t) = self.server.poll_transmit() {
                self.out.push(t);
            }
            self.out.reverse();
        }
        self.out.pop()
    }

    fn events(&mut self) {
        while let Some((addr, ev)) = self.server.poll_event() {
            match ev {
                Event::Connected => {
                    tracing::info!(target: "biller", %addr, "zone connected");
                    self.zones.insert(addr, ZoneConn::default());
                }
                Event::Packet(p) => self.packet(addr, &p),
                Event::Disconnected(reason) => {
                    tracing::info!(target: "biller", %addr, ?reason, "zone disconnected");
                    self.zone_gone(addr);
                }
                _ => {}
            }
        }
    }

    fn send(&mut self, to: SocketAddr, pkt: &BillerToServer) {
        self.server.send(&to, &pkt.to_bytes(), true);
    }

    fn reply(&mut self, zone: SocketAddr, cid: u32, text: &str) {
        let pkt = BillerToServer::ArenaMessage {
            connection_id: cid,
            text: to_latin1(text).into_iter().take(249).collect(),
        };
        self.send(zone, &pkt);
    }

    fn zone_gone(&mut self, addr: SocketAddr) {
        let now = self.now();
        if let Some(z) = self.zones.remove(&addr) {
            for o in z.players.values() {
                self.end_session(o, now);
            }
        }
    }

    fn end_session(&self, o: &Online, now: u64) {
        let secs = u32::try_from(now.saturating_sub(o.since) / 1000).unwrap_or(u32::MAX);
        if let Err(e) = self.store.add_seconds(o.user_id, secs) {
            tracing::warn!(target: "biller", "saving play time failed: {e}");
        }
    }

    fn packet(&mut self, addr: SocketAddr, data: &[u8]) {
        // logoffs come without the score when it didn't change
        let padded;
        let data = if data.first() == Some(&0x05) && data.len() == 15 {
            padded = [data, &[0u8; 14][..]].concat();
            &padded[..]
        } else {
            data
        };
        let pkt = match ServerToBiller::decode(data) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(target: "biller", %addr, "bad packet: {e}");
                return;
            }
        };
        let logged_in = self.zones.get(&addr).is_some_and(|z| z.logged_in);
        match pkt {
            ServerToBiller::ServerConnect {
                server_id,
                score_id,
                name,
                password,
                ..
            } => {
                let pw = latin1(password.as_bytes());
                if self.cfg.password.as_ref().is_some_and(|p| *p != pw) {
                    tracing::warn!(target: "biller", %addr, "zone sent the wrong password");
                    self.server.disconnect(&addr);
                    self.zones.remove(&addr);
                    return;
                }
                let name = latin1(name.as_bytes());
                tracing::info!(target: "biller", %addr, server_id, score_id, "zone logged in: {name}");
                let z = self.zones.entry(addr).or_default();
                z.logged_in = true;
                z.server_id = server_id;
                z.score_id = score_id;
                z.name = name;
                let id = BillerToServer::Identity(self.cfg.identity.clone());
                self.send(addr, &id);
            }
            _ if !logged_in => {
                tracing::debug!(target: "biller", %addr, "packet before the zone logged in");
            }
            ServerToBiller::Ping => {}
            ServerToBiller::ServerDisconnect => {
                self.zone_gone(addr);
                self.server.disconnect(&addr);
            }
            ServerToBiller::ServerCapabilities { flags } => {
                if let Some(z) = self.zones.get_mut(&addr) {
                    z.multicast = flags & 1 != 0;
                }
            }
            ServerToBiller::UserLogin(l) => self.login(addr, &l),
            ServerToBiller::UserLogoff {
                connection_id,
                score,
                ..
            } => {
                let now = self.now();
                let has_score = data.len() >= 29 && score != PlayerScore::default();
                let Some(z) = self.zones.get_mut(&addr) else {
                    return;
                };
                let score_id = z.score_id;
                if let Some(o) = z.players.remove(&connection_id) {
                    self.end_session(&o, now);
                    if has_score
                        && let Err(e) = self.store.set_score(o.user_id, score_id, &db_score(&score))
                    {
                        tracing::warn!(target: "biller", "saving score failed: {e}");
                    }
                }
            }
            ServerToBiller::UserBanner {
                connection_id,
                banner,
            } => {
                if let Some(o) = self.online(addr, connection_id) {
                    let _ = self.store.set_banner(o.user_id, &banner.0);
                }
            }
            ServerToBiller::UserScore {
                connection_id,
                score,
            } => {
                let score_id = self.zones.get(&addr).map_or(0, |z| z.score_id);
                if let Some(o) = self.online(addr, connection_id) {
                    let _ = self.store.set_score(o.user_id, score_id, &db_score(&score));
                }
            }
            ServerToBiller::UserDemographics {
                connection_id,
                data,
            } => {
                if let Some(o) = self.online(addr, connection_id) {
                    let _ = self.store.set_demographics(o.user_id, &data);
                }
            }
            ServerToBiller::UserCommand {
                connection_id,
                text,
            } => self.command(addr, connection_id, &latin1(&text)),
            ServerToBiller::UserChannelChat {
                connection_id,
                channel,
                text,
            } => self.channel_chat(addr, connection_id, &latin1(channel.as_bytes()), &text),
            ServerToBiller::UserRemotePrivate { sound, text, .. } => {
                self.remote_private(addr, sound, &text);
            }
            other => {
                tracing::debug!(target: "biller", %addr, "ignored packet {other:?}");
            }
        }
    }

    fn online(&self, zone: SocketAddr, cid: u32) -> Option<Online> {
        self.zones.get(&zone)?.players.get(&cid).cloned()
    }

    fn find_online(&self, name: &str) -> Option<(SocketAddr, u32, &Online)> {
        self.zones.iter().find_map(|(a, z)| {
            z.players
                .iter()
                .find(|(_, o)| o.name.eq_ignore_ascii_case(name))
                .map(|(c, o)| (*a, *c, o))
        })
    }

    fn login(&mut self, addr: SocketAddr, l: &oss_proto::billing::UserLogin) {
        let name = latin1(l.name.as_bytes()).trim().to_owned();
        let password = latin1(l.password.as_bytes());
        let cid = l.connection_id;
        let deny = |result: u8| {
            BillerToServer::UserLogin(BillerLogin {
                result,
                connection_id: cid,
                name: fixed(&name),
                ..BillerLogin::default()
            })
        };
        if name.is_empty()
            || name.len() > 23
            || name.starts_with(|c: char| !c.is_ascii_alphanumeric())
        {
            self.send(addr, &deny(login_result::BAD_NAME));
            return;
        }
        let now = unix_now();
        let user = match self.store.user(&name) {
            Ok(Some(u)) => {
                if u.banned {
                    self.send(addr, &deny(login_result::BANNED));
                    return;
                }
                if !self.store.check_password(u.id, &password).unwrap_or(false) {
                    self.send(addr, &deny(login_result::INVALID_PASSWORD));
                    return;
                }
                u
            }
            Ok(None) => {
                let create = match self.cfg.registration {
                    Registration::Auto => true,
                    Registration::Ask => l.make_new != 0,
                    Registration::Closed => false,
                };
                if self.cfg.registration == Registration::Closed {
                    self.send(addr, &deny(login_result::NO_NEW_CONNECTIONS));
                    return;
                }
                if !create {
                    self.send(addr, &deny(login_result::NEW_USER));
                    return;
                }
                match self.store.create_user(&name, &password, now) {
                    Ok(u) => {
                        tracing::info!(target: "biller", "new account {name} ({})", u.id);
                        u
                    }
                    Err(e) => {
                        tracing::warn!(target: "biller", "creating account failed: {e}");
                        self.send(addr, &deny(login_result::BUSY));
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "biller", "account lookup failed: {e}");
                self.send(addr, &deny(login_result::BUSY));
                return;
            }
        };
        // one session per account: drop the old one
        let old: Vec<(SocketAddr, u32)> = self
            .zones
            .iter()
            .flat_map(|(a, z)| {
                z.players
                    .iter()
                    .filter(|(_, o)| o.user_id == user.id)
                    .map(|(c, _)| (*a, *c))
            })
            .collect();
        let ms = self.now();
        for (a, c) in old {
            if let Some(o) = self.zones.get_mut(&a).and_then(|z| z.players.remove(&c)) {
                self.end_session(&o, ms);
            }
            self.send(
                a,
                &BillerToServer::Kick {
                    connection_id: c,
                    reason: 0,
                },
            );
        }
        let score_id = self.zones.get(&addr).map_or(0, |z| z.score_id);
        let score = self
            .store
            .score(user.id, score_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut banner = [0u8; 96];
        if user.banner.len() == 96 {
            banner.copy_from_slice(&user.banner);
        }
        let chats = user
            .chats
            .split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        if let Some(z) = self.zones.get_mut(&addr) {
            z.players.insert(
                cid,
                Online {
                    user_id: user.id,
                    name: user.name.clone(),
                    squad: user.squad.clone(),
                    chats,
                    since: ms,
                },
            );
        }
        let pkt = BillerToServer::UserLogin(BillerLogin {
            result: login_result::OK,
            connection_id: cid,
            name: fixed(&user.name),
            squad: fixed(&user.squad),
            banner: Raw(banner),
            seconds_played: user.seconds,
            first_login: db::civil(user.created),
            user_id: user.id,
            score: Some(score_of(&score)),
            ..BillerLogin::default()
        });
        self.send(addr, &pkt);
    }

    fn channel_chat(&mut self, addr: SocketAddr, cid: u32, channel: &str, text: &[u8]) {
        let Some(o) = self.online(addr, cid) else {
            return;
        };
        let chat = match channel.parse::<usize>() {
            Ok(n) if n >= 1 => o.chats.get(n - 1),
            _ => o
                .chats
                .iter()
                .find(|c| channel_display(c).eq_ignore_ascii_case(channel)),
        };
        let Some(chat) = chat else {
            self.reply(
                addr,
                cid,
                "You are not on that chat channel. Use ?chat=name to join one.",
            );
            return;
        };
        let key = channel_key(addr, chat);
        let mut msg = to_latin1(&o.name);
        msg.extend_from_slice(b"> ");
        msg.extend_from_slice(text);
        msg.truncate(249);
        let mut per_zone: HashMap<SocketAddr, Vec<(u32, u8)>> = HashMap::new();
        for (za, z) in &self.zones {
            for (c, p) in &z.players {
                if *za == addr && *c == cid {
                    continue;
                }
                if let Some(i) = p.chats.iter().position(|x| channel_key(*za, x) == key) {
                    per_zone.entry(*za).or_default().push((*c, (i + 1) as u8));
                }
            }
        }
        for (za, rcpts) in per_zone {
            if self.zones.get(&za).is_some_and(|z| z.multicast) {
                let pkt = BillerToServer::MultiChannelChat {
                    recipients: rcpts,
                    text: msg.clone(),
                };
                self.send(za, &pkt);
            } else {
                for (c, n) in rcpts {
                    let pkt = BillerToServer::ChannelMessage {
                        connection_id: c,
                        channel: n,
                        text: msg.clone(),
                    };
                    self.send(za, &pkt);
                }
            }
        }
    }

    fn remote_private(&mut self, addr: SocketAddr, sound: u8, text: &[u8]) {
        let t = latin1(text);
        let Some((target, rest)) = t.strip_prefix(':').and_then(|r| r.split_once(':')) else {
            return;
        };
        let source_server = self.zones.get(&addr).map_or(0, |z| z.server_id);
        let pkt = BillerToServer::ZoneMessage {
            source_server,
            kind: 2,
            sound,
            text: text.to_vec(),
        };
        if let Some(squad) = target.strip_prefix('#') {
            let zones: Vec<SocketAddr> = self
                .zones
                .iter()
                .filter(|(_, z)| {
                    z.players
                        .values()
                        .any(|o| o.squad.eq_ignore_ascii_case(squad))
                })
                .map(|(a, _)| *a)
                .collect();
            for z in zones {
                self.send(z, &pkt);
            }
        } else if let Some((za, _, _)) = self.find_online(target) {
            self.send(za, &pkt);
        } else {
            // tell the sender, found by the name in "(sender)>"
            let sender = rest
                .strip_prefix('(')
                .and_then(|r| r.split_once(")>"))
                .map(|(s, _)| s.to_owned());
            let cid = sender.and_then(|s| {
                self.zones.get(&addr).and_then(|z| {
                    z.players
                        .iter()
                        .find(|(_, o)| o.name.eq_ignore_ascii_case(&s))
                        .map(|(c, _)| *c)
                })
            });
            if let Some(c) = cid {
                self.reply(addr, c, &format!("{target} is not online."));
            }
        }
    }

    fn command(&mut self, addr: SocketAddr, cid: u32, text: &str) {
        let Some(o) = self.online(addr, cid) else {
            return;
        };
        let line = text.strip_prefix('?').unwrap_or(text);
        let (name, params) = split_command(line);
        let params = params.trim();
        let mut out: Vec<String> = Vec::new();
        match name.as_str() {
            "chat" => {
                if !params.is_empty() || line.len() > 4 {
                    let chats: Vec<String> = params
                        .split(',')
                        .map(|c| c.trim().chars().take(31).collect::<String>())
                        .filter(|c: &String| !channel_display(c).trim().is_empty())
                        .take(MAX_CHANNELS)
                        .collect();
                    let _ = self.store.set_chats(o.user_id, &chats.join(","));
                    if let Some(p) = self
                        .zones
                        .get_mut(&addr)
                        .and_then(|z| z.players.get_mut(&cid))
                    {
                        p.chats = chats;
                    }
                }
                let chats = self.online(addr, cid).map(|o| o.chats).unwrap_or_default();
                if chats.is_empty() {
                    out.push("You are not on any chat channels.".into());
                }
                for c in &chats {
                    let key = channel_key(addr, c);
                    let key = key.as_str();
                    let mut members: Vec<&str> = self
                        .zones
                        .iter()
                        .flat_map(|(za, z)| {
                            z.players
                                .values()
                                .filter(move |p| p.chats.iter().any(|x| channel_key(*za, x) == key))
                                .map(|p| p.name.as_str())
                        })
                        .collect();
                    members.sort_unstable_by_key(|m| m.to_ascii_lowercase());
                    out.push(format!("{}: {}", channel_display(c), members.join(",")));
                }
            }
            "find" => {
                let who = params;
                match self.find_online(who) {
                    Some((za, _, p)) => {
                        let zn = self
                            .zones
                            .get(&za)
                            .map(|z| z.name.clone())
                            .unwrap_or_default();
                        out.push(format!("{} is in {zn}", p.name));
                    }
                    None => out.push(format!("{who} is not online.")),
                }
            }
            "password" => {
                if params.is_empty() {
                    out.push("Usage: ?password=newpassword".into());
                } else {
                    let ok = self.store.set_password(o.user_id, params).is_ok();
                    out.push(
                        if ok {
                            "Password changed."
                        } else {
                            "Password not changed."
                        }
                        .into(),
                    );
                }
            }
            "squad" => {
                let who = if params.is_empty() {
                    o.name.as_str()
                } else {
                    params
                };
                match self.store.user(who) {
                    Ok(Some(u)) if !u.squad.is_empty() => {
                        out.push(format!("{} is in squad {}", u.name, u.squad));
                    }
                    Ok(Some(u)) => out.push(format!("{} is not in a squad.", u.name)),
                    _ => out.push(format!("Unknown player {who}.")),
                }
            }
            "squadcreate" | "squadjoin" => {
                let Some((sq, pw)) = params.split_once(':') else {
                    out.push(format!("Usage: ?{name}=squadname:password"));
                    self.reply_lines(addr, cid, &out);
                    return;
                };
                let sq = sq.trim();
                let current = self.store.user_by_id(o.user_id).ok().flatten();
                if current.as_ref().is_some_and(|u| !u.squad.is_empty()) {
                    out.push("You are already in a squad; ?squadleave first.".into());
                } else if sq.is_empty() || sq.len() > 23 {
                    out.push("Squad names are 1 to 23 characters.".into());
                } else if name == "squadcreate" {
                    match self.store.squad(sq) {
                        Ok(Some(_)) => out.push("That squad already exists.".into()),
                        Ok(None) => {
                            let ok = self
                                .store
                                .create_squad(sq, pw, o.user_id, unix_now())
                                .is_ok();
                            out.push(if ok {
                                format!("Squad {sq} created. It shows from your next login.")
                            } else {
                                "Squad not created.".into()
                            });
                        }
                        Err(_) => out.push("Squad not created.".into()),
                    }
                } else {
                    match self.store.squad(sq) {
                        Ok(Some(s))
                            if self
                                .store
                                .check_squad_password(&s.name, pw)
                                .unwrap_or(false) =>
                        {
                            let _ = self.store.set_squad(o.user_id, &s.name);
                            out.push(format!(
                                "You joined {}. It shows from your next login.",
                                s.name
                            ));
                        }
                        Ok(Some(_)) => out.push("Wrong squad password.".into()),
                        _ => out.push("No such squad.".into()),
                    }
                }
            }
            "squadleave" | "squadgrant" | "squadkick" | "squadpassword" | "squaddissolve" => {
                let Some(me) = self.store.user_by_id(o.user_id).ok().flatten() else {
                    return;
                };
                let squad = self.store.squad(&me.squad).ok().flatten();
                let Some(squad) = squad.filter(|_| !me.squad.is_empty()) else {
                    out.push("You are not in a squad.".into());
                    self.reply_lines(addr, cid, &out);
                    return;
                };
                let owner = squad.owner == me.id;
                match name.as_str() {
                    "squadleave" => {
                        let others = self
                            .store
                            .squad_members(&squad.name)
                            .unwrap_or_default()
                            .len()
                            > 1;
                        if owner && others {
                            out.push(
                                "You own this squad: ?squadgrant=player or ?squaddissolve first."
                                    .into(),
                            );
                        } else if owner {
                            let _ = self.store.dissolve_squad(&squad.name);
                            out.push(format!("You left {}; it was dissolved.", squad.name));
                        } else {
                            let _ = self.store.set_squad(me.id, "");
                            out.push(format!("You left {}.", squad.name));
                        }
                    }
                    _ if !owner => out.push("Only the squad owner can do that.".into()),
                    "squadgrant" | "squadkick" => match self.store.user(params) {
                        Ok(Some(u))
                            if u.squad.eq_ignore_ascii_case(&squad.name) && u.id != me.id =>
                        {
                            if name == "squadgrant" {
                                let _ = self.store.set_squad_owner(&squad.name, u.id);
                                out.push(format!("{} now owns {}.", u.name, squad.name));
                            } else {
                                let _ = self.store.set_squad(u.id, "");
                                out.push(format!("{} was removed from {}.", u.name, squad.name));
                            }
                        }
                        _ => out.push(format!("{params} is not another member of your squad.")),
                    },
                    "squadpassword" => {
                        if params.is_empty() {
                            out.push("Usage: ?squadpassword=newpassword".into());
                        } else {
                            let _ = self.store.set_squad_password(&squad.name, params);
                            out.push("Squad password changed.".into());
                        }
                    }
                    _ => {
                        let _ = self.store.dissolve_squad(&squad.name);
                        out.push(format!("{} was dissolved.", squad.name));
                    }
                }
            }
            "squadlist" => {
                let sq = if params.is_empty() {
                    self.store
                        .user_by_id(o.user_id)
                        .ok()
                        .flatten()
                        .map(|u| u.squad)
                        .unwrap_or_default()
                } else {
                    params.to_owned()
                };
                match self.store.squad(&sq) {
                    Ok(Some(s)) if !sq.is_empty() => {
                        let members = self.store.squad_members(&s.name).unwrap_or_default();
                        let marked: Vec<String> = members
                            .iter()
                            .map(|m| {
                                if self.find_online(m).is_some() {
                                    format!("{m}(online)")
                                } else {
                                    m.clone()
                                }
                            })
                            .collect();
                        out.push(format!("{}: {}", s.name, marked.join(", ")));
                    }
                    _ => out.push("No such squad.".into()),
                }
            }
            _ => {}
        }
        self.reply_lines(addr, cid, &out);
    }

    fn reply_lines(&mut self, addr: SocketAddr, cid: u32, lines: &[String]) {
        for l in lines {
            self.reply(addr, cid, l);
        }
    }
}

/// Runs a biller on `socket` until `stop` is set.
pub fn run(mut biller: Biller, socket: &UdpSocket, stop: &AtomicBool) -> std::io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_millis(10)))?;
    let mut buf = vec![0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        // errors are timeouts, and on Windows ICMP "port unreachable" from a
        // zone that went away
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            biller.handle_datagram(from, &buf[..n]);
        }
        biller.tick();
        while let Some((to, d)) = biller.poll_transmit() {
            if let Err(e) = socket.send_to(&d, to) {
                tracing::trace!(target: "biller", %to, "send failed: {e}");
            }
        }
    }
    Ok(())
}

/// A biller running on its own thread.
pub struct BillerHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BillerHandle {
    /// Binds `addr` and starts a biller there.
    pub fn spawn(addr: SocketAddr, biller: Biller) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        let addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("biller".into())
            .spawn(move || {
                if let Err(e) = run(biller, &socket, &s) {
                    tracing::error!(target: "biller", "biller stopped: {e}");
                }
            })?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    /// The address it listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops the biller and waits for it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for BillerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels() {
        let z: SocketAddr = ([127, 0, 0, 1], 5000).into();
        assert_eq!(channel_key(z, "Pub"), "pub");
        assert_eq!(channel_key(z, "$l$x|Pub"), "127.0.0.1:5000/$l$x|pub");
        assert_eq!(channel_display("$s$secret|Staff"), "Staff");
        assert_eq!(channel_display("plain"), "plain");
        assert_eq!(split_command("chat=a,b"), ("chat".into(), "a,b"));
        assert_eq!(split_command("SQUADLIST"), ("squadlist".into(), ""));
    }
}
