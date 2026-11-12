//! The four small ASSS modules: `buy` (`?buy`), `game_timer` (`?timer` and
//! friends), `watchdamage` (C2S 0x32) and `ap_multipub` (several public
//! arenas).

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::{DamageData, chat_type};
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new(arena_conf: &str, global: &str, modules: Option<&[&str]>) -> Self {
        let src = MemSource::new([
            ("conf/global.conf", global.to_owned()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_buy = 1\ncmd_time = 1\ncmd_timer = 1\ncmd_timereset = 1\n\
                 cmd_pausetimer = 1\nprivcmd_watchdamage = 1\ncmd_watchdamage = 1\n\
                 cmd_watchwatchdamage = 1\ncmd_go = 1\n"
                    .to_owned(),
            ),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", arena_conf.to_owned()),
        ]);
        let mut b = ServerBuilder::with_source(src);
        if let Some(m) = modules {
            b = b.modules(m.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>());
        }
        let mut zone = b.build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
        }
    }

    fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    fn send(&mut self, p: PlayerId, data: &[u8]) {
        self.zone.handle_packet(p, data);
        self.lo.collect(&mut self.zone);
    }

    fn player_as(&mut self, name: &str, kind: ClientKind, arena: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, kind, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: if kind == ClientKind::Continuum {
                c2s::kind::CONT_LOGIN
            } else {
                c2s::kind::LOGIN
            },
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
            cont_id: (kind == ClientKind::Continuum).then_some([0u8; 64]),
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: if arena.is_empty() { -1 } else { -3 },
            arena: FixedStr::new(arena.as_bytes()),
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(20);
        p
    }

    fn player(&mut self, name: &str) -> PlayerId {
        self.player_as(name, ClientKind::Vie, "")
    }

    fn cmd(&mut self, p: PlayerId, text: &str) -> Vec<String> {
        self.cmd_to(p, 0, chat_type::PUBLIC, text)
    }

    fn cmd_to(&mut self, p: PlayerId, pid: i16, kind: u8, text: &str) -> Vec<String> {
        self.lo.take(p);
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind,
            sound: 0,
            pid,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.send(p, &buf);
        self.texts(p)
    }

    fn texts(&mut self, p: PlayerId) -> Vec<String> {
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .map(|(d, _)| {
                String::from_utf8_lossy(&S2CChat::decode(&d).expect("chat").0.text).into_owned()
            })
            .collect()
    }

    fn arena_name(&self, p: PlayerId) -> Option<String> {
        self.zone
            .players
            .get(p)
            .and_then(|x| x.arena)
            .and_then(|a| self.zone.arenas.get(a))
            .map(|a| a.name.clone())
    }
}

/// The default module set plus `watchdamage`, which ASSS doesn't load by
/// default either.
fn with_watchdamage() -> Vec<&'static str> {
    let mut m: Vec<&str> = oss_server::modules::DEFAULT_MODULES.to_vec();
    m.push("watchdamage");
    m
}

const BUY_ARENA: &str = "[Modules]\nAttachModules = buy\n\
[Cost]\nRepel = 100\nBurst = 250\n[Team]\nDesiredTeams = 2\n";

/// Gives a player points to spend.
fn give_points(h: &mut H, p: PlayerId, points: i32) {
    let stats = h
        .zone
        .iface::<dyn oss_server::modules::stats::Stats>()
        .expect("stats");
    stats.increment(
        &mut h.zone,
        p,
        oss_server::modules::stats::stat::KILL_POINTS,
        points,
    );
}

/// A position packet, optionally inside a safe zone.
fn position(h: &mut H, p: PlayerId, safe: bool) {
    // the server keeps only the newest position, so move the clock on
    h.tick(1);
    let mut buf = Vec::new();
    oss_proto::position::C2SPosition {
        time: h.t as u32,
        x: 8000,
        y: 8000,
        status: if safe { 0x20 } else { 0 },
        ..Default::default()
    }
    .encode(&mut buf);
    h.send(p, &buf);
}

#[test]
fn buy_lists_and_sells() {
    let mut h = H::new(BUY_ARENA, "", None);
    let p = h.player("alice");
    let out = h.cmd(p, "?buy");
    assert!(out.iter().any(|t| t == "buy: Repel        100"), "{out:?}");
    assert!(out.iter().any(|t| t == "buy: Burst        250"), "{out:?}");
    assert!(
        !out.iter().any(|t| t.contains("Thor")),
        "an item with no cost isn't for sale: {out:?}"
    );

    // a spectator can't buy
    let out = h.cmd(p, "?buy rep");
    assert!(
        out.iter().any(|t| t == "Spectators cannot purchase items."),
        "{out:?}"
    );

    h.send(p, &[c2s::kind::SET_SHIP, 0]);
    position(&mut h, p, false);
    let out = h.cmd(p, "?buy rep");
    assert!(
        out.iter()
            .any(|t| t == "You must be in a safe zone to purchase items."),
        "{out:?}"
    );

    position(&mut h, p, true);
    let out = h.cmd(p, "?buy rep");
    assert!(
        out.iter()
            .any(|t| t == "You don't have enough points to purchase that item."),
        "{out:?}"
    );

    give_points(&mut h, p, 150);
    let out = h.cmd(p, "?buy rep");
    assert!(out.iter().any(|t| t == "Bought Repel."), "{out:?}");
    // paid for out of flag points, so the kill points stand
    let stats = h
        .zone
        .iface::<dyn oss_server::modules::stats::Stats>()
        .expect("stats");
    assert_eq!(
        stats.get(&h.zone, p, oss_server::modules::stats::stat::KILL_POINTS),
        150
    );
    assert_eq!(
        stats.get(&h.zone, p, oss_server::modules::stats::stat::FLAG_POINTS),
        -100
    );

    let out = h.cmd(p, "?buy nosuchthing");
    assert!(
        out.iter()
            .any(|t| t == "Invalid item specified for purchase."),
        "{out:?}"
    );
    let out = h.cmd(p, "?buy thor");
    assert!(
        out.iter()
            .any(|t| t == "That item isn't available for purchase."),
        "{out:?}"
    );
}

#[test]
fn buy_anywhere_when_the_arena_allows_it() {
    let conf = format!("{BUY_ARENA}[Cost]\nPurchaseAnytime = 1\n");
    let mut h = H::new(&conf, "", None);
    let p = h.player("alice");
    h.send(p, &[c2s::kind::SET_SHIP, 0]);
    position(&mut h, p, false);
    give_points(&mut h, p, 500);
    let out = h.cmd(p, "?buy burst");
    assert!(out.iter().any(|t| t == "Bought Burst."), "{out:?}");
}

#[test]
fn a_timer_set_by_hand_counts_down_and_pauses() {
    let mut h = H::new("[Team]\nDesiredTeams = 2\n", "", None);
    let p = h.player("alice");
    let out = h.cmd(p, "?time");
    assert!(
        out.iter().any(|t| t == "Time left: 0 minutes 0 seconds"),
        "{out:?}"
    );
    let out = h.cmd(p, "?timer 1:30");
    assert!(
        out.iter().any(|t| t == "Time left: 1 minutes 30 seconds"),
        "{out:?}"
    );
    h.tick(3000); // 30 s
    let out = h.cmd(p, "?time");
    assert!(
        out.iter().any(|t| t == "Time left: 1 minutes 0 seconds"),
        "{out:?}"
    );
    let out = h.cmd(p, "?pausetimer");
    assert!(
        out.iter()
            .any(|t| t == "Timer paused at:  1 minutes 0 seconds"),
        "{out:?}"
    );
    h.tick(3000);
    let out = h.cmd(p, "?time");
    assert!(
        out.iter()
            .any(|t| t == "Timer paused at:  1 minutes 0 seconds"),
        "a paused timer doesn't run: {out:?}"
    );
    let out = h.cmd(p, "?pausetimer");
    assert!(
        out.iter()
            .any(|t| t == "Timer resumed at: 1 minutes 0 seconds"),
        "{out:?}"
    );
    // bad arguments say so
    let out = h.cmd(p, "?timer soon");
    assert!(
        out.iter()
            .any(|t| t == "timer format is: '?timer mins[:secs]'"),
        "{out:?}"
    );
}

#[test]
fn a_timed_game_warns_and_starts_again() {
    let conf = "[Misc]\nTimedGame = 500\nTimerWarnings = 3, 2\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, "", None);
    let p = h.player("alice");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let s = seen.clone();
    h.zone.on::<oss_server::modules::game_timer::TimesUp>(
        oss_server::registry::Owner(999),
        oss_server::registry::Scope::Global,
        move |_, _| *s.lock().expect("lock") += 1,
    );
    // ?timer can't override a timed game
    let out = h.cmd(p, "?timer 5");
    assert!(
        out.iter()
            .any(|t| t == "Timer is fixed to Misc:TimedGame setting."),
        "{out:?}"
    );
    h.lo.take(p);
    // the clock started when the arena was created, a few ticks before the
    // player finished logging in
    h.tick(600);
    let out = h.texts(p);
    assert!(
        out.iter().any(|t| t == "NOTICE: 3 seconds remaining."),
        "{out:?}"
    );
    assert!(
        out.iter().any(|t| t == "NOTICE: 2 seconds remaining."),
        "{out:?}"
    );
    assert!(out.iter().any(|t| t == "NOTICE: Game over"), "{out:?}");
    assert_eq!(*seen.lock().expect("lock"), 1);
    // and the next game is already running
    let out = h.cmd(p, "?time");
    assert!(
        out.iter().any(|t| t.starts_with("Time left: 0 minutes")),
        "{out:?}"
    );
    h.tick(600);
    assert_eq!(*seen.lock().expect("lock"), 2);
}

#[test]
fn watchdamage_relays_reports_to_watchers() {
    let mut h = H::new("[Team]\nDesiredTeams = 2\n", "", Some(&with_watchdamage()));
    let alice = h.player("alice");
    // damage watching needs the watched player to be on Continuum
    let bob = h.player_as("bob", ClientKind::Continuum, "");
    h.send(bob, &[c2s::kind::SET_SHIP, 0]);

    let damage = |h: &mut H, p: PlayerId| {
        let mut buf = Vec::new();
        c2s::C2SDamage {
            time: h.t as u32,
            entries: vec![DamageData::default()],
        }
        .encode(&mut buf);
        h.send(p, &buf);
    };

    // nobody is watching: no S2C 0x38 goes anywhere
    damage(&mut h, bob);
    assert!(
        !h.lo
            .take(alice)
            .iter()
            .any(|(d, _)| d[0] == s2c::kind::DAMAGE)
    );

    // alice watches bob: his client is told to report
    h.lo.take(bob);
    let out = h.cmd_to(alice, bob.0 as i16, chat_type::PRIVATE, "?watchdamage 1");
    assert!(
        out.iter().any(|t| t == "Damage watching on bob turned on"),
        "{out:?}"
    );
    let told = h.lo.take(bob);
    let toggle = told
        .iter()
        .find(|(d, _)| d[0] == s2c::kind::TOGGLE_DAMAGE)
        .expect("0x37");
    assert_eq!(s2c::ToggleDamage::decode(&toggle.0).expect("decode").on, 1);

    damage(&mut h, bob);
    let got = h.lo.take(alice);
    let relayed = got
        .iter()
        .find(|(d, _)| d[0] == s2c::kind::DAMAGE)
        .expect("0x38");
    let relayed = s2c::S2CDamage::decode(&relayed.0).expect("decode");
    assert_eq!(relayed.pid, bob.0 as i16);
    assert_eq!(relayed.entries.len(), 1);

    // and off again, which stops his client reporting
    h.lo.take(bob);
    let out = h.cmd_to(alice, bob.0 as i16, chat_type::PRIVATE, "?watchdamage 0");
    assert!(
        out.iter().any(|t| t == "Damage watching on bob turned off"),
        "{out:?}"
    );
    let told = h.lo.take(bob);
    let toggle = told
        .iter()
        .find(|(d, _)| d[0] == s2c::kind::TOGGLE_DAMAGE)
        .expect("0x37");
    assert_eq!(s2c::ToggleDamage::decode(&toggle.0).expect("decode").on, 0);
    damage(&mut h, bob);
    assert!(
        !h.lo
            .take(alice)
            .iter()
            .any(|(d, _)| d[0] == s2c::kind::DAMAGE)
    );
}

#[test]
fn watchdamage_needs_continuum_and_clears_in_public() {
    let mut h = H::new("[Team]\nDesiredTeams = 2\n", "", Some(&with_watchdamage()));
    let alice = h.player("alice");
    let vie = h.player("bob");
    let out = h.cmd_to(alice, vie.0 as i16, chat_type::PRIVATE, "?watchdamage 1");
    assert!(
        out.iter()
            .any(|t| t == "watchdamage requires bob to use Continuum"),
        "{out:?}"
    );
    let cont = h.player_as("carol", ClientKind::Continuum, "");
    h.cmd_to(alice, cont.0 as i16, chat_type::PRIVATE, "?watchdamage 1");
    assert_eq!(
        oss_server::modules::watchdamage::watch_count(&h.zone, cont),
        1
    );
    let out = h.cmd(alice, "?watchdamage 0");
    assert!(
        out.iter().any(|t| t == "All damage watching turned off"),
        "{out:?}"
    );
    assert_eq!(
        oss_server::modules::watchdamage::watch_count(&h.zone, cont),
        0
    );
}

#[test]
fn ap_multipub_spreads_players_over_public_arenas() {
    let mut modules: Vec<&str> = oss_server::modules::DEFAULT_MODULES.to_vec();
    modules.push("ap_multipub");
    let conf = "[General]\nDesiredPlaying = 2\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, "[General]\nPublicArenas = pub\n", Some(&modules));
    // the first two fill pub1
    let a = h.player("alice");
    h.send(a, &[c2s::kind::SET_SHIP, 0]);
    assert_eq!(h.arena_name(a).as_deref(), Some("pub1"));
    let b = h.player("bob");
    h.send(b, &[c2s::kind::SET_SHIP, 0]);
    assert_eq!(h.arena_name(b).as_deref(), Some("pub1"));
    // the third finds it full and opens pub2
    let c = h.player("carol");
    assert_eq!(h.arena_name(c).as_deref(), Some("pub2"));
}

#[test]
fn without_the_setting_public_placement_is_unchanged() {
    let mut modules: Vec<&str> = oss_server::modules::DEFAULT_MODULES.to_vec();
    modules.push("ap_multipub");
    let mut h = H::new("[Team]\nDesiredTeams = 2\n", "", Some(&modules));
    let a = h.player("alice");
    assert_eq!(h.arena_name(a).as_deref(), Some("0"));
}
