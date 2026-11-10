//! `game_timer` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/game-timer.txt`, from `core/game_timer.c`
//! compiled unmodified).
//!
//! `Misc:TimedGame` is in ticks, not seconds. A warning fires only on the
//! check where the seconds remaining EQUAL the configured value, so a check
//! that steps over one misses it silently, and only the first five values
//! are ever read. While paused, `timeout` holds a duration rather than a
//! deadline — the same field meaning two things, as in ASSS.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::arenaman::{ArenaAction, ArenaActionKind};
use oss_server::modules::core as core_mod;
use oss_server::modules::game_timer;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/game-timer.txt");

struct H {
    zone: Zone,
    lo: Loopback,
    arena: ArenaId,
    p: PlayerId,
}

impl H {
    fn new(timed_game: u64, warnings: &str) -> Self {
        let conf = format!(
            "[Modules]\nAttachModules = game_timer\n\
             [Misc]\nTimedGame = {timed_game}\nTimerWarnings = {warnings}\n\
             [Team]\nDesiredTeams = 2\n"
        );
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_time = 1\ncmd_timer = 1\ncmd_timereset = 1\n\
                 cmd_pausetimer = 1\n"
                    .to_owned(),
            ),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", conf),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let mut h = Self {
            zone,
            lo: Loopback::default(),
            arena: ArenaId(0),
            p: PlayerId(0),
        };
        h.p = h.join("alice");
        h.arena = h
            .zone
            .players
            .get(h.p)
            .and_then(|x| x.arena)
            .expect("arena");
        h
    }

    fn at(&mut self, t: u64) {
        self.zone.set_now(Ticks(t));
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
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
            cont_id: None,
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        for t in 1..=5 {
            self.zone.set_now(Ticks(t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
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
        for t in 6..=20 {
            self.zone.set_now(Ticks(t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
        p
    }

    fn command(&mut self, text: &str) {
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: chat_type::PUBLIC,
            sound: 0,
            pid: 0,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.zone.handle_packet(self.p, &buf);
        self.lo.collect(&mut self.zone);
    }

    fn said(&mut self) -> Vec<(u8, String)> {
        let p = self.p;
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .filter_map(|(d, _)| S2CChat::decode(&d).ok())
            .map(|c| (c.0.sound, String::from_utf8_lossy(&c.0.text).into_owned()))
            .collect()
    }
}

/// `TimedGame 0 TimerWarnings [120,60 30] t0 1000` into its three parts.
fn scenario(spec: &str) -> (u64, String, u64) {
    let open = spec.find('[').expect("warnings open");
    let close = spec.find(']').expect("warnings close");
    let warnings = spec[open + 1..close].to_owned();
    let head: Vec<&str> = spec[..open].split_whitespace().collect();
    let timed: u64 = head
        .iter()
        .position(|w| *w == "TimedGame")
        .and_then(|i| head.get(i + 1))
        .and_then(|v| v.parse().ok())
        .expect("TimedGame");
    let tail: Vec<&str> = spec[close + 1..].split_whitespace().collect();
    let t0: u64 = tail
        .iter()
        .position(|w| *w == "t0")
        .and_then(|i| tail.get(i + 1))
        .and_then(|v| v.parse().ok())
        .expect("t0");
    (timed, warnings, t0)
}

#[test]
fn game_timer_matches_asss() {
    let mut h: Option<H> = None;
    let mut label = String::new();
    let mut said: Vec<(u8, String)> = Vec::new();
    let mut chat_at = 0usize;
    let mut step = 0usize;
    let mut checked = 0usize;
    let mut scenarios = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            if let Some((text, spec)) = c.rsplit_once(": ")
                && spec.contains("TimedGame")
            {
                let (timed, warnings, t0) = scenario(spec);
                label = text.to_owned();
                let mut hh = H::new(timed, &warnings);
                hh.at(t0);
                let _ = hh.said();
                h = Some(hh);
                scenarios += 1;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let kind = match word {
            "at" => {
                let hh = h.as_mut().expect("a scenario first");
                hh.at(rest.trim().parse().expect("tick"));
                continue;
            }
            other => other,
        };
        match kind {
            "conf" => {
                // the transcript edits the arena's config between actions
                let hh = h.as_mut().expect("a scenario first");
                let mut it = rest.split_whitespace();
                let (key, value) = (it.next().expect("key"), it.next().expect("value"));
                let arena = hh.arena;
                let file = hh
                    .zone
                    .arenas
                    .get(arena)
                    .map(|a| a.cfg.clone())
                    .expect("cfg");
                let next = hh.zone.cfg.set(&file, "Misc", key, value, None, false);
                if let Some(a) = hh.zone.arenas.get_mut(arena) {
                    a.cfg = next;
                }
            }
            "create" | "confchanged" | "destroy" => {
                let hh = h.as_mut().expect("a scenario first");
                let action = match kind {
                    "create" => ArenaActionKind::Create,
                    "confchanged" => ArenaActionKind::ConfChanged,
                    _ => ArenaActionKind::Destroy,
                };
                let arena = hh.arena;
                hh.zone.fire(&ArenaAction { arena, action });
                hh.lo.collect(&mut hh.zone);
                said = hh.said();
                chat_at = 0;
                step += 1;
            }
            "tick" => {
                let hh = h.as_mut().expect("a scenario first");
                game_timer::check_timers(&mut hh.zone);
                hh.lo.collect(&mut hh.zone);
                said = hh.said();
                chat_at = 0;
                step += 1;
            }
            "timer" | "time" | "timereset" | "pausetimer" => {
                let hh = h.as_mut().expect("a scenario first");
                let text = if rest.is_empty() {
                    format!("?{kind}")
                } else {
                    format!("?{kind} {rest}")
                };
                hh.command(&text);
                said = hh.said();
                chat_at = 0;
                step += 1;
            }
            "state" => {
                let hh = h.as_ref().expect("a scenario first");
                let v: Vec<u64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let d = game_timer::timer_state(&hh.zone, hh.arena);
                let at = format!("{label}: step {step}");
                assert_eq!(u64::from(d.enabled), v[0], "{at}: enabled");
                assert_eq!(d.game_len, v[1], "{at}: gamelen");
                assert_eq!(d.timeout, v[2], "{at}: timeout");
                checked += 1;
            }
            "chat" => {
                let at = format!("{label}: step {step}");
                let (_who, rest) = rest.split_once(' ').unwrap_or((rest, ""));
                let (sound, text) = rest.split_once(' ').unwrap_or((rest, ""));
                let want_sound: u8 = sound.parse().unwrap_or(0);
                let got = said.get(chat_at).cloned();
                assert_eq!(
                    got,
                    Some((want_sound, text.to_owned())),
                    "{at}: line {chat_at}, said {said:?}"
                );
                chat_at += 1;
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(scenarios >= 9, "only {scenarios} scenarios replayed");
    assert!(checked > 60, "only {checked} records checked");
}
