//! `koth` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/koth.txt`, from `core/koth.c` compiled unmodified).
//!
//! The subtle rule is the one about recovery: a kill that removes the
//! victim's LAST crown does not count as a crown kill, because ASSS re-tests
//! the victim's crown after taking it away. With `DeathCount` 0 that makes
//! `CrownRecoverKills` unreachable, and the transcript carries both cases
//! side by side to prove it.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::game::Kill;
use oss_server::modules::koth;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, Ship};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/koth.txt");

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
}

impl H {
    fn new(settings: &str, ships: u32, count: usize) -> Self {
        let conf = format!(
            "[Modules]\nAttachModules = koth\n[King]\n{settings}\n[Team]\nDesiredTeams = 2\n"
        );
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", conf),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let mut h = Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            players: Vec::new(),
        };
        for i in 0..count {
            let p = h.join(&format!("p{i}"));
            if i == 0 {
                h.arena = h.zone.players.get(p).and_then(|x| x.arena).expect("arena");
            }
            h.players.push(p);
        }
        // the ship bitmap says who is flying and who is spectating
        for i in 0..count {
            let p = h.players[i];
            let flying = ships & (1 << i) != 0;
            if let Some(x) = h.zone.players.get_mut(p) {
                x.ship = if flying { Ship(0) } else { Ship::SPEC };
            }
        }
        h
    }

    fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
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
        self.tick(5);
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
        self.tick(10);
        p
    }
}

/// `ExpireTime 18000 DeathCount 0 CrownRecoverKills 0 MinPlaying 3, 6 players
/// (ship bitmap 0xf)` into a `[King]` section, a bitmap and a head count.
fn scenario(spec: &str) -> (String, u32, usize) {
    let (keys, tail) = spec.split_once(',').unwrap_or((spec, ""));
    let mut section = String::new();
    let mut words = keys.split_whitespace();
    while let Some(k) = words.next() {
        if let Some(v) = words.next() {
            section.push_str(&format!("{k} = {v}\n"));
        }
    }
    let count: usize = tail
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .expect("player count");
    let bitmap = tail
        .split_once("0x")
        .and_then(|(_, hex)| {
            let digits: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
            u32::from_str_radix(&digits, 16).ok()
        })
        .expect("ship bitmap");
    (section, bitmap, count)
}

#[test]
fn koth_matches_asss() {
    let mut h: Option<H> = None;
    let mut label = String::new();
    let mut states = 0usize;
    let mut scenarios = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            if let Some((text, spec)) = c.rsplit_once(": ") {
                let (section, ships, count) = scenario(spec);
                label = text.to_owned();
                h = Some(H::new(&section, ships, count));
                scenarios += 1;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "start" => {
                let hh = h.as_mut().expect("a scenario first");
                let arena = hh.arena;
                koth::start_game(&mut hh.zone, arena);
            }
            "check" => {
                let hh = h.as_mut().expect("a scenario first");
                let arena = hh.arena;
                koth::check_game(&mut hh.zone, arena);
            }
            "kill" => {
                let hh = h.as_mut().expect("a scenario first");
                let v: Vec<usize> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let (killer, killed) = (hh.players[v[0]], hh.players[v[1]]);
                let arena = hh.arena;
                hh.zone.fire(&Kill {
                    arena,
                    killer,
                    killed,
                    bounty: 0,
                    flags: 0,
                    points: std::cell::Cell::new(0),
                    green: std::cell::Cell::new(0),
                });
            }
            "state" => {
                let hh = h.as_ref().expect("a scenario first");
                let mut fields = rest.split_whitespace();
                let tag = fields.next().unwrap_or("");
                for f in fields {
                    // <index>:[spec,]<crown>/<hadcrown>/<deaths>/<crownkills>/<flag>
                    let (idx, body) = f.split_once(':').expect("player field");
                    let i: usize = idx.parse().expect("player index");
                    let body = body.strip_prefix("spec,").unwrap_or(body);
                    let v: Vec<&str> = body.split('/').collect();
                    let p = hh.players[i];
                    let c = koth::crown_state(&hh.zone, p);
                    let at = format!("{label}: {tag} p{i}");
                    assert_eq!(c.crown, v[0] == "1", "{at}: crown");
                    assert_eq!(c.had_crown, v[1] == "1", "{at}: hadcrown");
                    assert_eq!(c.deaths.to_string(), v[2], "{at}: deaths");
                    assert_eq!(c.crown_kills.to_string(), v[3], "{at}: crownkills");
                    let flag = hh
                        .zone
                        .players
                        .get(p)
                        .is_some_and(|x| koth::crown_bit(x) != 0);
                    assert_eq!(flag, v[4] == "flag", "{at}: crown bit in player data");
                    states += 1;
                }
            }
            _ => {}
        }
    }
    assert!(scenarios >= 9, "only {scenarios} scenarios replayed");
    assert!(states > 60, "only {states} player states checked");
}
