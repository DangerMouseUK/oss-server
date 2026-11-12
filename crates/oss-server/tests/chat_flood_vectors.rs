//! Chat flood control and chat masks replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/chat-flood.txt`, from `core/chat.c` compiled
//! unmodified).
//!
//! The one that costs a player their voice for a whole session: a moderator's
//! session-long mask has no expiry, and the flood path only sets an expiry
//! when there already is one or when there is no mask at all. With a mask and
//! no expiry neither branch runs, so the nine flood bits are OR'd in and
//! never expire, and nothing anywhere records why.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::capman::Capabilities;
use oss_server::modules::chat;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/chat-flood.txt");

/// Answers only the one capability the transcript turns on and off.
/// Driving it through groupdef would mean rewriting a config file in the
/// middle of a scenario; the transcript's own harness just makes capman
/// answer differently, and so does this.
struct SpamCap(Arc<AtomicBool>);

impl Capabilities for SpamCap {
    fn has_capability(&self, _zone: &mut Zone, _player: PlayerId, cap: &str) -> bool {
        cap == chat::CAP_CAN_SPAM && self.0.load(Ordering::Relaxed)
    }
    fn has_capability_in_arena(
        &self,
        zone: &mut Zone,
        player: PlayerId,
        _arena: ArenaId,
        cap: &str,
    ) -> bool {
        self.has_capability(zone, player, cap)
    }
    fn has_capability_by_name(&self, _zone: &mut Zone, _name: &str, cap: &str) -> bool {
        cap == chat::CAP_CAN_SPAM && self.0.load(Ordering::Relaxed)
    }
    fn higher_than(&self, _zone: &mut Zone, _a: PlayerId, _b: PlayerId) -> bool {
        false
    }
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
    spam: Arc<AtomicBool>,
    /// The clock the scenario starts at, restored after everyone is in.
    base: (u64, u64),
}

impl H {
    fn new(settings: &HashMap<String, i64>) -> Self {
        let g = |k: &str| settings.get(k).copied().unwrap_or(0);
        let global = format!(
            "[Chat]\nFloodLimit = {}\nFloodShutup = {}\n",
            g("floodlimit"),
            g("floodshutup")
        );
        let arena = format!(
            "[Chat]\nRestrictChat = {}\n[Team]\nDesiredTeams = 2\n",
            g("restrictchat")
        );
        let src = MemSource::new([
            ("conf/global.conf", global),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", arena),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let spam = Arc::new(AtomicBool::new(g("canspam") != 0));
        zone.reg.provide::<dyn Capabilities>(
            Owner(997),
            Scope::Global,
            Arc::new(SpamCap(spam.clone())),
        );
        let mut h = Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            players: Vec::new(),
            spam,
            base: (0, 0),
        };
        h.base = (
            u64::try_from(g("ticks")).unwrap_or(0),
            u64::try_from(g("wall")).unwrap_or(0),
        );
        h.at(
            u64::try_from(g("ticks")).unwrap_or(0),
            u64::try_from(g("wall")).unwrap_or(0),
        );
        h
    }

    /// Sets both clocks. The transcript drives them separately, because the
    /// flood counter decays on ticks while a mask expires on wall time.
    fn at(&mut self, ticks: u64, wall: u64) {
        self.t = ticks;
        self.zone.set_now(Ticks(ticks));
        self.zone.res.insert(chat::WallClock(wall));
        self.lo.collect(&mut self.zone);
    }

    fn join(&mut self, name: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: 1,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            bound_ip: 0,
            permission_id: 1,
            server_ip: 0,
            bound_port: 0,
            redirect_id: 0,
            using_proxy: 0,
            unused: 0,
            cont_id: None,
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.run(5);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.run(10);
        p
    }

    /// Advances the lifecycle. The clock has to move for a player to get
    /// through it, so `seat` puts the scenario's clock back afterwards.
    fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    /// Seats `n` players and clears whatever entering left behind, which is
    /// what the transcript's PA_PREENTERARENA does at seat time.
    fn seat(&mut self, n: usize) {
        for i in 0..n {
            let p = self.join(&format!("p{i}"));
            if i == 0 {
                self.arena = self
                    .zone
                    .players
                    .get(p)
                    .and_then(|x| x.arena)
                    .expect("arena");
            }
            self.players.push(p);
        }
        let (ticks, wall) = self.base;
        self.at(ticks, wall);
        for i in 0..self.players.len() {
            let p = self.players[i];
            chat::clear_mask(&mut self.zone, p);
            let _ = self.lo.take(p);
        }
    }
}

fn kv(line: &str) -> HashMap<String, i64> {
    line.split_whitespace()
        .skip(1)
        .filter_map(|w| w.split_once('='))
        .filter_map(|(k, v)| Some((k.to_owned(), v.parse().ok()?)))
        .collect()
}

#[test]
fn chat_flood_matches_asss() {
    let mut h: Option<H> = None;
    let mut label = String::new();
    let mut last_comment = String::new();
    let mut wall = 0u64;
    let mut checked = 0usize;
    let mut scenarios = 0usize;
    let mut flood_bits: u16 = 0;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            last_comment = c.to_owned();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "floodbits" => {
                flood_bits = u16::from_str_radix(rest.trim(), 16).expect("flood bits");
            }
            "settings" => {
                label = std::mem::take(&mut last_comment);
                let s = kv(line);
                wall = u64::try_from(s.get("wall").copied().unwrap_or(0)).unwrap_or(0);
                h = Some(H::new(&s));
                scenarios += 1;
            }
            "seat" => {
                let hh = h.as_mut().expect("settings first");
                hh.seat(rest.trim().parse().expect("player count"));
            }
            "at" => {
                let hh = h.as_mut().expect("settings first");
                let s = kv(&format!("at {rest}"));
                wall = u64::try_from(s.get("wall").copied().unwrap_or(0)).unwrap_or(0);
                let ticks = u64::try_from(s.get("ticks").copied().unwrap_or(0)).unwrap_or(0);
                hh.at(ticks, wall);
            }
            "flood" => {
                let hh = h.as_mut().expect("settings first");
                let mut it = rest.split_whitespace();
                let who: usize = it.next().and_then(|s| s.parse().ok()).expect("player");
                let n: usize = it
                    .next()
                    .and_then(|s| s.strip_prefix('x')?.parse().ok())
                    .unwrap_or(1);
                let p = hh.players[who];
                for _ in 0..n {
                    // ASSS runs expire_mask and then check_flood for every
                    // message; player_chat_mask is the expiry half
                    let _ = chat::player_chat_mask(&mut hh.zone, p);
                    chat::check_flood(&mut hh.zone, p);
                }
            }
            "expire" => {
                let hh = h.as_mut().expect("settings first");
                let who: usize = rest.trim().parse().expect("player");
                let p = hh.players[who];
                let _ = chat::player_chat_mask(&mut hh.zone, p);
            }
            "preenter" => {
                let hh = h.as_mut().expect("settings first");
                let who: usize = rest.trim().parse().expect("player");
                let p = hh.players[who];
                let arena = hh.arena;
                hh.zone.fire(&oss_server::modules::core::PlayerAction {
                    player: p,
                    action: oss_server::modules::core::Action::PreEnterArena,
                    arena: Some(arena),
                });
            }
            "canspam" => {
                let hh = h.as_mut().expect("settings first");
                hh.spam.store(rest.trim() == "1", Ordering::Relaxed);
            }
            "setmask" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let mask = u16::from_str_radix(v[1], 16).expect("mask");
                let timeout: u64 = v[2]
                    .strip_prefix("timeout=")
                    .and_then(|t| t.parse().ok())
                    .expect("timeout");
                let p = hh.players[who];
                chat::set_player_chat_mask(&mut hh.zone, p, mask, timeout);
            }
            "getmask" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let want = u16::from_str_radix(v[2], 16).expect("mask");
                let p = hh.players[who];
                let got = chat::player_chat_mask(&mut hh.zone, p);
                assert_eq!(got, want, "{label}: GetPlayerChatMask for p{who}");
                checked += 1;
            }
            "state" => {
                let hh = h.as_ref().expect("settings first");
                for item in rest.split_whitespace() {
                    let Some((who, body)) = item.split_once(':') else {
                        continue;
                    };
                    let v: Vec<&str> = body.split('/').collect();
                    if who == "arena" {
                        let got = chat::arena_chat_mask(&hh.zone, hh.arena);
                        assert_eq!(format!("{got:04x}"), v[0], "{label}: arena mask");
                        checked += 1;
                        continue;
                    }
                    let i: usize = who.parse().expect("player index");
                    let p = hh.players[i];
                    let st = chat::mask_state(&hh.zone, p);
                    let at = format!("{label}: p{i}");
                    assert_eq!(format!("{:04x}", st.mask), v[0], "{at} mask");
                    // the record carries seconds remaining, not an absolute
                    let left = if st.expires == 0 {
                        0
                    } else {
                        st.expires.saturating_sub(wall)
                    };
                    assert_eq!(left.to_string(), v[1], "{at} seconds until expiry");
                    assert_eq!(st.msgs.to_string(), v[2], "{at} message count");
                    assert_eq!(st.last_check.to_string(), v[3], "{at} lastcheck");
                    checked += 4;
                }
            }
            _ => {}
        }
    }
    assert_eq!(flood_bits, 0x16be, "the flood mask bits moved");
    assert!(scenarios >= 9, "only {scenarios} scenarios replayed");
    assert!(checked > 90, "only {checked} records checked");
}
