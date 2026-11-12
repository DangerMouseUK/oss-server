//! `buy` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/buy.txt`, from `core/buy.c` compiled unmodified).
//!
//! The rule worth pinning is the item lookup: ASSS runs `strstr` over its
//! table in table order with no `break`, so the LAST entry whose string
//! occurs anywhere in the parameters wins, case-sensitively. `thorax` buys
//! Thor, `exit` buys XRadar, `report` buys Portal and `Bomb` matches nothing.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, PrizeReceive, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::stats::{Stats, stat};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, Ship};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/buy.txt");
const SAFE_ZONE: u8 = 0x20;

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    p: PlayerId,
}

impl H {
    fn new(cost_section: &str) -> Self {
        let conf = format!(
            "[Modules]\nAttachModules = buy\n[Cost]\n{cost_section}\n[Team]\nDesiredTeams = 2\n"
        );
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\ncmd_buy = 1\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", conf),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let mut h = Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            p: PlayerId(0),
        };
        h.p = h.join("alice");
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

    fn set_points(&mut self, kill: i32, flag: i32) {
        let stats = self.zone.iface::<dyn Stats>().expect("stats");
        let p = self.p;
        stats.set(&mut self.zone, p, stat::KILL_POINTS, kill);
        stats.set(&mut self.zone, p, stat::FLAG_POINTS, flag);
    }

    fn points(&self) -> (i32, i32) {
        let stats = self.zone.iface::<dyn Stats>().expect("stats");
        (
            stats.get(&self.zone, self.p, stat::KILL_POINTS),
            stats.get(&self.zone, self.p, stat::FLAG_POINTS),
        )
    }

    /// Runs `?buy <params>` and returns the prize sent and the lines said.
    fn buy(&mut self, params: &str) -> (Option<(i16, i16)>, Vec<String>) {
        let p = self.p;
        let _ = self.lo.take(p);
        let text = if params.is_empty() {
            "?buy".to_owned()
        } else if params.trim().is_empty() {
            // The transcript drives buy.c directly, so it can hand it an
            // argument of nothing but spaces. No chat line can: both this
            // server's command splitter and ASSS's skip spaces and '=' and
            // nothing else before the parameters. A tab survives both
            // intact and is the same thing as far as buy.c is concerned --
            // neither contains any item string.
            "?buy \t".to_owned()
        } else {
            format!("?buy {params}")
        };
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: chat_type::PUBLIC,
            sound: 0,
            pid: 0,
            text: text.into_bytes(),
        })
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.lo.collect(&mut self.zone);
        let out = self.lo.take(p);
        let prize = out
            .iter()
            .find(|(d, _)| d[0] == s2c::kind::PRIZE_RECEIVE)
            .and_then(|(d, _)| PrizeReceive::decode(d).ok())
            .map(|z| (z.prize, z.count));
        let chat = out
            .iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .filter_map(|(d, _)| S2CChat::decode(d).ok())
            .map(|c| String::from_utf8_lossy(&c.0.text).into_owned())
            .collect();
        (prize, chat)
    }
}

/// `PurchaseAnytime 0 XRadar=100 ...` into `[Cost]` lines.
fn cost_section(spec: &str) -> String {
    let mut out = String::new();
    let mut words = spec.split_whitespace();
    while let Some(w) = words.next() {
        match w.split_once('=') {
            Some((k, v)) => out.push_str(&format!("{k} = {v}\n")),
            None => {
                if let Some(v) = words.next() {
                    out.push_str(&format!("{w} = {v}\n"));
                }
            }
        }
    }
    out
}

#[test]
fn buy_matches_asss() {
    let mut h: Option<H> = None;
    let mut label = String::new();
    let mut prize: Option<(i16, i16)> = None;
    let mut said: Vec<String> = Vec::new();
    let mut tries = 0usize;
    let mut checked = 0usize;
    let mut chat_at = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            // the last ": " separates the label from the [Cost] settings
            if let Some((text, costs)) = c.rsplit_once(": ") {
                label = text.to_owned();
                h = Some(H::new(&cost_section(costs)));
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            // the table, restated in the transcript for a reader's benefit
            "item" => {}
            "try" => {
                let hh = h.as_mut().expect("a scenario first");
                // try [<params>] <ship|spec> <safe> <kill points> <flag points>
                let open = rest.find('[').expect("open bracket");
                let close = rest.rfind(']').expect("close bracket");
                let params = rest[open + 1..close].to_owned();
                let tail: Vec<&str> = rest[close + 1..].split_whitespace().collect();
                let in_ship = tail[0] == "ship";
                let safe = tail[1] == "1";
                let kill: i32 = tail[2].parse().expect("kill points");
                let flag: i32 = tail[3].parse().expect("flag points");
                let p = hh.p;
                if let Some(x) = hh.zone.players.get_mut(p) {
                    x.ship = if in_ship { Ship(0) } else { Ship::SPEC };
                    x.pos.status = if safe { SAFE_ZONE } else { 0 };
                }
                hh.set_points(kill, flag);
                let (z, c) = hh.buy(&params);
                prize = z;
                said = c;
                chat_at = 0;
                tries += 1;
            }
            "prize" => {
                let at = format!("{label}: purchase {tries}");
                let v: Vec<&str> = rest.split_whitespace().collect();
                if v[0] == "-" {
                    assert!(prize.is_none(), "{at}: sent {prize:?}, ASSS sent none");
                } else {
                    let want = (
                        v[0].parse::<i16>().expect("prize"),
                        v[1].parse::<i16>().expect("count"),
                    );
                    assert_eq!(prize, Some(want), "{at}: prize");
                }
                checked += 1;
            }
            "stat" => {
                let hh = h.as_ref().expect("a scenario first");
                let at = format!("{label}: purchase {tries}");
                let v: Vec<i32> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                assert_eq!(hh.points(), (v[0], v[1]), "{at}: kill and flag points");
                checked += 1;
            }
            "chat" => {
                let at = format!("{label}: purchase {tries}");
                let got = said.get(chat_at).map(String::as_str);
                assert_eq!(got, Some(rest), "{at}: line {chat_at}, said {said:?}");
                chat_at += 1;
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(tries >= 30, "only {tries} purchases replayed");
    assert!(checked > 90, "only {checked} records checked");
}
