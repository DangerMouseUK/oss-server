//! `turf_reward` and `points_turf_reward` replayed against ASSS's own
//! transcript (`fixtures/asss-vectors/turf-reward.txt`, produced by
//! compiling ASSS 1.4.4's `turf/turf_reward.c` and `turf/points_turf_reward.c`
//! unmodified).
//!
//! Each scenario starts from the settings `loadSettings` resolved, so the
//! replay sets them directly rather than going back through the config
//! parser, exactly as the vector's header invites. Every `ding` record runs
//! the real reward sequence and the records after it are assertions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::s2c::{self, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::fg_turf::TurfTag;
use oss_server::modules::game::Kill;
use oss_server::modules::stats::{Stats, stat};
use oss_server::modules::turf_reward::{
    Award, RecoveryCutoff, RewardStyle, TurfData, TurfPostReward, TurfSettings, WeightCalc,
};
use oss_server::net::Loopback;
use oss_server::player::{PlayerId, PlayerStatus, Ship};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/turf-reward.txt");

/// Safe-zone bit in a position packet's status byte.
const SAFE_ZONE: u8 = 0x20;
/// Far enough ahead that the module's own timer never fires: the vector
/// drives every ding itself.
const NEVER: Ticks = Ticks(u64::MAX / 2);

fn tile_list(n: usize) -> Vec<u8> {
    let mut b = Vec::new();
    for i in 0..n as u32 {
        // turf flag tiles, spread out so flagcore gives each its own id
        let (x, y) = (100 + i * 7, 100 + i * 11);
        b.extend_from_slice(&(x | (y << 12) | (170u32 << 24)).to_le_bytes());
    }
    b
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
    dinged: Arc<Mutex<Vec<(TurfData, Award, bool)>>>,
    /// Flag points each player had before the ding being checked.
    before: Vec<i32>,
}

impl H {
    fn new(flags: usize) -> Self {
        let conf = "[General]\nMap = test.lvl\n\
            [Modules]\nAttachModules = fg_turf turf_reward points_turf_reward\n\
            [TurfReward]\nRewardStyle = TR_STYLE_STANDARD\n\
            [Team]\nDesiredTeams = 2\n";
        let mut src = MemSource::new([
            ("conf/global.conf", ""),
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            ("conf/groupdef.conf", "[default]\n"),
            ("conf/staff.conf", ""),
            ("arenas/(default)/arena.conf", conf),
        ]);
        src.insert("maps/test.lvl", tile_list(flags));
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let dinged: Arc<Mutex<Vec<(TurfData, Award, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let d = dinged.clone();
        zone.on::<TurfPostReward>(Owner(998), Scope::Global, move |_, ev| {
            d.lock()
                .expect("lock")
                .push((ev.data.clone(), ev.state.award, ev.state.update));
        });
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            players: Vec::new(),
            dinged,
            before: Vec::new(),
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

    fn join(&mut self, name: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, oss_server::player::ClientKind::Vie, None)
            .expect("slot");
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
        self.tick(5);
        p
    }

    /// Puts a player exactly where the `seat` record says.
    fn seat(&mut self, p: PlayerId, freq: u16, in_ship: bool, place: char) {
        if let Some(x) = self.zone.players.get_mut(p) {
            x.freq = freq;
            x.ship = if in_ship { Ship(0) } else { Ship::SPEC };
            x.status = if place == 'e' {
                PlayerStatus::ArenaRespAndCbs
            } else {
                PlayerStatus::Playing
            };
            x.pos.status = if place == 'z' { SAFE_ZONE } else { 0 };
        }
    }

    fn data(&mut self) -> &mut TurfData {
        let arena = self.arena;
        self.zone
            .arenas
            .get_mut(arena)
            .and_then(|a| a.ext.get_mut::<TurfData>())
            .expect("turf data")
    }

    /// Stops the module's own timer firing, so only `ding` records ding.
    /// A no-op before anyone has joined, since the arena is made on demand.
    fn hold_timer(&mut self) {
        let arena = self.arena;
        if let Some(d) = self
            .zone
            .arenas
            .get_mut(arena)
            .and_then(|a| a.ext.get_mut::<TurfData>())
        {
            d.next_ding = NEVER;
        }
    }

    fn flag_points(&self, p: PlayerId) -> i32 {
        self.zone
            .iface::<dyn Stats>()
            .map_or(0, |s| s.get(&self.zone, p, stat::FLAG_POINTS))
    }

    fn snapshot_points(&mut self) {
        self.before = self.players.iter().map(|&p| self.flag_points(p)).collect();
    }

    fn texts(&mut self, p: PlayerId) -> Vec<(u8, String)> {
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .filter_map(|(d, _)| S2CChat::decode(&d).ok())
            .map(|c| (c.0.sound, String::from_utf8_lossy(&c.0.text).into_owned()))
            .collect()
    }
}

fn settings_from(line: &str) -> (TurfSettings, usize) {
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for word in line.split_whitespace().skip(1) {
        if let Some((k, v)) = word.split_once('=') {
            kv.insert(k, v);
        }
    }
    let n = |k: &str| -> i64 {
        kv.get(k)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("settings key {k} missing in {line}"))
    };
    let style = match n("style") {
        0 => RewardStyle::Disabled,
        1 => RewardStyle::Periodic,
        2 => RewardStyle::Standard,
        3 => RewardStyle::StdBty,
        4 => RewardStyle::Weights,
        5 => RewardStyle::FixedPts,
        // the transcript uses 99 for "a style the module does not know"
        _ => RewardStyle::Other,
    };
    let weights: Vec<i32> = kv
        .get("weights")
        .map(|w| w.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| vec![1]);
    let s = TurfSettings {
        reward_style: style,
        reward_modifier: n("modifier") as i32,
        max_points: n("maxpoints") as i32,
        min_players_on_team: n("minplayersteam") as i32,
        min_players_in_arena: n("minplayersarena") as i32,
        min_teams: n("minteams") as i32,
        min_flags: n("minflags") as i32,
        min_percent_flags: n("minpctflags") as f64,
        min_weights: n("minweights"),
        min_percent_weights: n("minpctweights") as f64,
        min_percent: n("minpct") as f64,
        spec_receive_points: n("spec") != 0,
        safe_receive_points: n("safe") != 0,
        weight_calc: if n("weightcalc") == 0 {
            WeightCalc::Dings
        } else {
            WeightCalc::Time
        },
        recovery_cutoff: match n("recovery") {
            0 => RecoveryCutoff::Dings,
            1 => RecoveryCutoff::Time,
            _ => RecoveryCutoff::DingsAndTime,
        },
        recover_dings: n("recoverdings").max(0) as u32,
        recover_time: n("recovertime").max(0) as u64,
        recover_max: n("recovermax") as i32,
        timer_initial: n("timerinitial").max(0) as u64,
        timer_interval: n("timerinterval").max(0) as u64,
        weights,
    };
    (s, n("flags") as usize)
}

/// `0w.0w.1s z` style triples: freq digit, ship/spec, and where they are.
fn seats(spec: &str) -> Vec<(u16, bool, char)> {
    let b: Vec<char> = spec.chars().collect();
    b.chunks(3)
        .filter(|c| c.len() == 3)
        .map(|c| (c[0].to_digit(10).unwrap_or(0) as u16, c[1] == 'w', c[2]))
        .collect()
}

/// Doubles are printed with `%.17g`, so they compare exactly once parsed;
/// this still allows a last-bit difference from a different order of
/// operations.
fn close(a: f64, b: f64, what: &str, label: &str) {
    assert!(
        (a - b).abs() <= 1e-9 * b.abs().max(1.0),
        "{label}: {what} is {a}, ASSS says {b}"
    );
}

#[test]
fn turf_reward_matches_asss() {
    let mut label = String::new();
    let mut pending: Option<TurfSettings> = None;
    let mut said: Vec<Vec<(u8, String)>> = Vec::new();
    let mut h: Option<H> = None;
    let mut last_comment = String::new();
    let mut ding = 0usize;
    let mut checked = 0usize;
    let mut scenarios = 0usize;

    for line in VECTORS.lines() {
        // only the line ending comes off: ASSS's " [MAX] " message really
        // does end in a space, and the transcript records it faithfully
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
            "settings" => {
                label = std::mem::take(&mut last_comment);
                scenarios += 1;
                let (settings, flags) = settings_from(line);
                pending = Some(settings);
                // the arena is made when the first player enters, so the
                // settings go in after the seat line
                h = Some(H::new(flags));
            }
            "seat" => {
                let hh = h.as_mut().expect("settings first");
                // everyone joins first: joining ticks the clock, which would
                // walk a player seated as "still entering" on into Playing
                let places = seats(rest);
                for i in 0..places.len() {
                    let p = hh.join(&format!("p{i}"));
                    if i == 0 {
                        hh.arena = hh.zone.players.get(p).and_then(|x| x.arena).expect("arena");
                    }
                    hh.players.push(p);
                }
                for (i, (freq, ship, place)) in places.into_iter().enumerate() {
                    let p = hh.players[i];
                    hh.seat(p, freq, ship, place);
                }
                if let Some(s) = pending.take() {
                    hh.data().settings = s;
                }
                hh.hold_timer();
                hh.snapshot_points();
                // joining drains nothing: clear the entry chatter
                for i in 0..hh.players.len() {
                    let p = hh.players[i];
                    let _ = hh.lo.take(p);
                }
            }
            "tag" => {
                let hh = h.as_mut().expect("settings first");
                let mut it = rest.split_whitespace();
                let fid: usize = it.next().and_then(|s| s.parse().ok()).expect("flag id");
                let who: usize = it.next().and_then(|s| s.parse().ok()).expect("player");
                let p = hh.players[who];
                let freq = hh.zone.players.get(p).map_or(0, |x| x.freq) as i16;
                let old = hh.data().flags.get(fid).map_or(-1, |f| f.freq);
                let arena = hh.arena;
                hh.zone.fire(&TurfTag {
                    arena,
                    player: p,
                    fid,
                    old_freq: old,
                    new_freq: freq,
                });
                hh.hold_timer();
            }
            "kill" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<usize> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let (killer, killed, bounty) = (hh.players[v[0]], hh.players[v[1]], v[2] as i16);
                let arena = hh.arena;
                hh.zone.fire(&Kill {
                    arena,
                    killer,
                    killed,
                    bounty,
                    flags: 0,
                    points: std::cell::Cell::new(0),
                    green: std::cell::Cell::new(0),
                });
                hh.hold_timer();
            }
            "advance" => {
                let hh = h.as_mut().expect("settings first");
                let n: u64 = rest.trim().parse().unwrap_or(0);
                hh.tick(n);
                hh.hold_timer();
            }
            "ding" => {
                let hh = h.as_mut().expect("settings first");
                hh.snapshot_points();
                hh.dinged.lock().expect("lock").clear();
                let arena = hh.arena;
                oss_server::modules::turf_reward::do_reward(&mut hh.zone, arena);
                hh.lo.collect(&mut hh.zone);
                hh.hold_timer();
                said.clear();
                for i in 0..hh.players.len() {
                    let p = hh.players[i];
                    said.push(hh.texts(p));
                }
                ding += 1;
            }
            "calc" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<i64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let at = format!("{label} ding {ding}");
                let got = hh.dinged.lock().expect("lock").first().cloned();
                if v[0] == 1 {
                    // TR_CALC_FAIL: doReward returns before the post-reward
                    // callback, so there is nothing to have captured
                    assert!(got.is_none(), "{at}: scored despite TR_CALC_FAIL");
                    checked += 1;
                    continue;
                }
                let (_, award, update) =
                    got.unwrap_or_else(|| panic!("{at}: no post-reward callback"));
                let want = match v[1] {
                    0 => Award::Player,
                    1 => Award::Team,
                    2 => Award::Both,
                    _ => Award::None,
                };
                assert_eq!(award, want, "{at}: award");
                assert_eq!(update, v[2] != 0, "{at}: update");
                checked += 1;
            }
            "arena" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<i64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let got = hh.dinged.lock().expect("lock").first().map(|x| x.0.clone());
                let blocked = v[0] == 1;
                if blocked {
                    assert!(got.is_none(), "{label} ding {ding}: scored despite blocked");
                    continue;
                }
                // On TR_CALC_FAIL there is no post-reward callback, but
                // doReward also returns before clearing the counters, so
                // what preCalc worked out is still sitting in the arena.
                let d = match got {
                    Some(d) => d,
                    None => hh.data().clone(),
                };
                let at = format!("{label} ding {ding}");
                assert_eq!(i64::from(d.num_players), v[1], "{at}: numPlayers");
                assert_eq!(
                    i64::from(d.num_valid_players),
                    v[2],
                    "{at}: numValidPlayers"
                );
                assert_eq!(i64::from(d.num_teams), v[3], "{at}: numTeams");
                assert_eq!(i64::from(d.num_valid_teams), v[4], "{at}: numValidTeams");
                close(
                    d.sum_per_capita_flags,
                    v[7] as f64,
                    "sumPerCapitaFlags",
                    &at,
                );
                assert_eq!(d.num_weights, v[6], "{at}: numWeights");
                close(
                    d.sum_per_capita_weights,
                    v[8] as f64,
                    "sumPerCapitaWeights",
                    &at,
                );
                assert_eq!(d.num_points, v[9], "{at}: numPoints");
                assert_eq!(i64::from(d.tags), v[10], "{at}: tags");
                assert_eq!(i64::from(d.steals), v[11], "{at}: steals");
                assert_eq!(i64::from(d.recoveries), v[12], "{at}: recoveries");
                assert_eq!(i64::from(d.lost), v[13], "{at}: lost");
                assert_eq!(i64::from(d.kills), v[14], "{at}: kills");
                assert_eq!(d.bounty_exchanged, v[15], "{at}: bountyExchanged");
                checked += 1;
            }
            "team" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<f64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let captured = hh.dinged.lock().expect("lock").first().map(|x| x.0.clone());
                let d = match captured {
                    Some(d) => d,
                    None => hh.data().clone(),
                };
                if d.teams.is_empty() {
                    continue;
                }
                let freq = v[0] as i16;
                let at = format!("{label} ding {ding} freq {freq}");
                let t = d
                    .teams
                    .iter()
                    .find(|t| t.freq == freq)
                    .unwrap_or_else(|| panic!("{at}: team missing"));
                assert_eq!(t.valid, v[1] != 0.0, "{at}: valid");
                assert_eq!(f64::from(t.num_flags), v[2], "{at}: numFlags");
                assert_eq!(f64::from(t.num_players), v[3], "{at}: numPlayers");
                assert_eq!(t.num_weights as f64, v[4], "{at}: numWeights");
                close(t.percent_flags, v[5], "percentFlags", &at);
                close(t.percent_weights, v[6], "percentWeights", &at);
                close(t.per_capita_flags, v[7], "perCapitaFlags", &at);
                close(t.per_capita_weights, v[8], "perCapitaWeights", &at);
                close(t.percent, v[9], "percent", &at);
                assert_eq!(f64::from(t.num_points), v[10], "{at}: numPoints");
                checked += 1;
            }
            "award" => {
                let hh = h.as_mut().expect("settings first");
                let at = format!("{label} ding {ding}");
                for item in rest.split_whitespace() {
                    let Some((who, pts)) = item.split_once(':') else {
                        continue;
                    };
                    let i: usize = who.parse().expect("player index");
                    let want: i32 = pts.parse().expect("points");
                    let got = hh.flag_points(hh.players[i]) - hh.before[i];
                    assert_eq!(
                        got, want,
                        "{at}: p{i} flag points moved by {got}, ASSS says {want}"
                    );
                }
                checked += 1;
            }
            "chat" => {
                let hh = h.as_mut().expect("settings first");
                let at = format!("{label} ding {ding}");
                let (who, rest) = rest.split_once(' ').unwrap_or((rest, ""));
                let (sound, text) = rest.split_once(' ').unwrap_or((rest, ""));
                let want_sound: u8 = sound.parse().unwrap_or(0);
                if who == "arena" {
                    for (i, lines) in said.iter().enumerate() {
                        assert!(
                            lines.iter().any(|(s, t)| *s == want_sound && t == text),
                            "{at}: p{i} never heard {text:?}, only {lines:?}"
                        );
                    }
                } else {
                    let i: usize = who.parse().expect("player index");
                    let lines = said.get(i).cloned().unwrap_or_default();
                    assert!(
                        lines.iter().any(|(s, t)| *s == want_sound && t == text),
                        "{at}: p{i} never heard {text:?}, only {lines:?}"
                    );
                }
                checked += 1;
                let _ = hh;
            }
            "flags" => {
                let hh = h.as_mut().expect("settings first");
                let at = format!("{label} ding {ding}");
                for item in rest.split_whitespace() {
                    let Some((id, body)) = item.split_once(':') else {
                        continue;
                    };
                    let id: usize = id.parse().expect("flag id");
                    // <freq|->/<tagger>/<dings>/<weight>/<recovered>[/old:...]
                    let head: Vec<&str> = body.split('/').collect();
                    let f = hh
                        .data()
                        .flags
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| panic!("{at}: flag {id} missing"));
                    let want_freq: i16 = if head[0] == "-" {
                        -1
                    } else {
                        head[0].parse().expect("freq")
                    };
                    assert_eq!(f.freq, want_freq, "{at}: flag {id} freq");
                    if want_freq < 0 {
                        // ASSS writes dings -1 on an unowned flag; ours is
                        // unsigned and stays 0. Nothing reads it: every
                        // weight calculation skips a flag nobody owns, and
                        // a tag either restores a saved value or sets 0.
                        continue;
                    }
                    if head.len() > 2 {
                        assert_eq!(f.dings.to_string(), head[2], "{at}: flag {id} dings");
                    }
                    if head.len() > 3 {
                        assert_eq!(f.weight.to_string(), head[3], "{at}: flag {id} weight");
                    }
                    if head.len() > 4 && !head[4].starts_with("old") {
                        assert_eq!(
                            f.recovered.to_string(),
                            head[4],
                            "{at}: flag {id} recovered"
                        );
                    }
                }
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(scenarios >= 25, "only {scenarios} scenarios replayed");
    assert!(checked > 100, "only {checked} records checked");
    let _ = h;
}
