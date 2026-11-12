//! `points_goal` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/soccer-goal.txt`, from `scoring/points_goal.c`
//! compiled unmodified), across all seven `Soccer:Mode` values.
//!
//! Two rules a port is likely to tidy up and get wrong. `RewardPoints`
//! counts teams with `freq[p_freq % 4]` — four buckets, not distinct freqs —
//! so freqs 0 and 4 are one team for `Soccer:MinTeams`. And `Soccer:WinBy`
//! 0, the default, makes an absolute-scoring game unable to end, because the
//! leader satisfies its own `score + WinBy <= score` test.

use std::collections::HashMap;

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::s2c::{self, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::balls::Goal;
use oss_server::modules::core as core_mod;
use oss_server::modules::points_goal;
use oss_server::modules::stats::{Stats, stat};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, PlayerStatus, Ship};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/soccer-goal.txt");
const SAFE_ZONE: u8 = 0x20;

const MODES: [&str; 7] = [
    "GOAL_ALL",
    "GOAL_LEFTRIGHT",
    "GOAL_TOPBOTTOM",
    "GOAL_CORNERS_3_1",
    "GOAL_CORNERS_1_3",
    "GOAL_SIDES_3_1",
    "GOAL_SIDES_1_3",
];

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
    /// Names have to stay unique: a parked player is still logged in, and a
    /// second login under the same name would not reach the arena.
    seq: usize,
}

impl H {
    fn new(settings: &HashMap<String, i64>) -> Self {
        let g = |k: &str| settings.get(k).copied().unwrap_or(0);
        let mode = MODES
            .get(usize::try_from(g("mode")).unwrap_or(0))
            .copied()
            .unwrap_or("GOAL_ALL");
        let freqtypes = if g("freqtypes") != 0 { 1 } else { 0 };
        let conf = format!(
            "[Modules]\nAttachModules = points_goal\n\
             [Soccer]\nMode = {mode}\nCapturePoints = {}\nReward = {}\nWinBy = {}\n\
             MinPlayers = {}\nMinTeams = {}\nBallCount = 1\n\
             [Misc]\nFrequencyShipTypes = {freqtypes}\n\
             [Team]\nDesiredTeams = 2\n",
            g("cpts"),
            g("reward"),
            g("winby"),
            g("minplayers"),
            g("minteams"),
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
            seq: 0,
        };
        // the arena is made on demand, and the transcript asserts the
        // starting scores before it seats anyone, so bring one player in to
        // create it and then park them outside so nothing counts them
        let boot = h.join("boot");
        h.arena = h
            .zone
            .players
            .get(boot)
            .and_then(|x| x.arena)
            .expect("arena");
        h.park(boot);
        h
    }

    /// Sits a player out. They stay in the arena -- taking the last player
    /// out would destroy it and the next join would make a different one --
    /// but as a spectator, which ASSS does not count for anything here.
    fn park(&mut self, p: PlayerId) {
        if let Some(x) = self.zone.players.get_mut(p) {
            x.ship = Ship::SPEC;
            x.pos.status = 0;
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

    /// Replaces the arena's players with what the seat record describes.
    fn seat(&mut self, spec: &str) {
        for p in self.players.clone() {
            self.park(p);
        }
        self.players.clear();
        let chars: Vec<char> = spec.chars().collect();
        let places: Vec<Vec<char>> = chars
            .chunks(3)
            .filter(|c| c.len() == 3)
            .map(<[char]>::to_vec)
            .collect();
        // everyone joins first: joining ticks the clock, which would walk a
        // player seated as "still entering" on into Playing again
        for _ in 0..places.len() {
            self.seq += 1;
            let name = format!("p{}", self.seq);
            let p = self.join(&name);
            self.players.push(p);
        }
        for (i, c) in places.into_iter().enumerate() {
            let p = self.players[i];
            if let Some(x) = self.zone.players.get_mut(p) {
                x.freq = c[0].to_digit(10).unwrap_or(0) as u16;
                x.ship = if c[1] == 'w' { Ship(0) } else { Ship::SPEC };
                x.status = if c[2] == 'e' {
                    PlayerStatus::ArenaRespAndCbs
                } else {
                    PlayerStatus::Playing
                };
                x.pos.status = if c[2] == 'z' { SAFE_ZONE } else { 0 };
            }
        }
        for i in 0..self.players.len() {
            let p = self.players[i];
            let _ = self.lo.take(p);
        }
    }

    fn stat_of(&self, p: PlayerId, which: u16) -> i32 {
        self.zone
            .iface::<dyn Stats>()
            .map_or(0, |s| s.get(&self.zone, p, which))
    }

    fn points(&self) -> Vec<i32> {
        self.players
            .iter()
            .map(|&p| self.stat_of(p, stat::FLAG_POINTS))
            .collect()
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
fn soccer_scoring_matches_asss() {
    let mut h: Option<H> = None;
    let mut label = String::new();
    let mut last_comment = String::new();
    let mut before: Vec<i32> = Vec::new();
    let mut said: Vec<Vec<(u8, String)>> = Vec::new();
    let mut arena_said: Vec<(u8, String)> = Vec::new();
    let mut chat_at: HashMap<String, usize> = HashMap::new();
    let mut checked = 0usize;
    let mut scenarios = 0usize;

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
            "settings" => {
                label = std::mem::take(&mut last_comment);
                h = Some(H::new(&kv(line)));
                scenarios += 1;
            }
            "seat" => {
                let hh = h.as_mut().expect("settings first");
                hh.seat(rest.trim());
                before = hh.points();
            }
            "reward" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<i64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                before = hh.points();
                let arena = hh.arena;
                let got = points_goal::reward_points(&mut hh.zone, arena, v[0] as i16);
                assert_eq!(i64::from(got), v[1], "{label}: RewardPoints returned");
                checked += 1;
            }
            "goal" => {
                let hh = h.as_mut().expect("settings first");
                let v: Vec<i64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                before = hh.points();
                for i in 0..hh.players.len() {
                    let p = hh.players[i];
                    let _ = hh.lo.take(p);
                }
                let arena = hh.arena;
                let player = hh.players[v[0] as usize];
                // the ball's owning freq is what MyGoal reads for a null goal
                oss_server::modules::balls::set_ball_freq(
                    &mut hh.zone,
                    arena,
                    v[1] as usize,
                    v[2] as i16,
                );
                hh.zone.fire(&Goal {
                    arena,
                    player,
                    ball: v[1] as usize,
                    x: v[3] as i16,
                    y: v[4] as i16,
                });
                hh.lo.collect(&mut hh.zone);
                said = Vec::new();
                for i in 0..hh.players.len() {
                    let p = hh.players[i];
                    let lines: Vec<(u8, String)> = hh
                        .lo
                        .take(p)
                        .into_iter()
                        .filter(|(d, _)| d[0] == s2c::kind::CHAT)
                        .filter_map(|(d, _)| S2CChat::decode(&d).ok())
                        .map(|c| (c.0.sound, String::from_utf8_lossy(&c.0.text).into_owned()))
                        .collect();
                    said.push(lines);
                }
                // an arena message reaches everyone, so take it from p0
                arena_said = said.first().cloned().unwrap_or_default();
                chat_at.clear();
            }
            "award" => {
                let hh = h.as_ref().expect("settings first");
                let now = hh.points();
                for item in rest.split_whitespace() {
                    let Some((who, pts)) = item.split_once(':') else {
                        continue;
                    };
                    let i: usize = who.parse().expect("player index");
                    let want: i32 = pts.parse().expect("points");
                    let got =
                        now.get(i).copied().unwrap_or(0) - before.get(i).copied().unwrap_or(0);
                    assert_eq!(got, want, "{label}: p{i} flag points moved by {got}");
                }
                checked += 1;
            }
            "ballstats" => {
                let hh = h.as_ref().expect("settings first");
                for item in rest.split_whitespace() {
                    let Some((who, body)) = item.split_once(':') else {
                        continue;
                    };
                    let i: usize = who.parse().expect("player index");
                    let (won, lost) = body.split_once('/').expect("won/lost");
                    let p = hh.players[i];
                    assert_eq!(
                        hh.stat_of(p, stat::BALL_GAMES_WON).to_string(),
                        won,
                        "{label}: p{i} ball games won"
                    );
                    assert_eq!(
                        hh.stat_of(p, stat::BALL_GAMES_LOST).to_string(),
                        lost,
                        "{label}: p{i} ball games lost"
                    );
                }
                checked += 1;
            }
            "score" => {
                let hh = h.as_mut().expect("settings first");
                let mut it = rest.split_whitespace();
                let tag = it.next().unwrap_or("");
                let want: Vec<i32> = it.by_ref().take(8).filter_map(|s| s.parse().ok()).collect();
                let ends: u32 = rest
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("endgames=")?.parse().ok())
                    .unwrap_or(0);
                let arena = hh.arena;
                let got = points_goal::score(&mut hh.zone, arena).expect("scores");
                assert_eq!(got.to_vec(), want, "{label}: {tag} scores");
                assert_eq!(
                    points_goal::end_games(&mut hh.zone, arena),
                    ends,
                    "{label}: {tag} games ended"
                );
                checked += 1;
            }
            "chat" => {
                let hh = h.as_ref().expect("settings first");
                let (who, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                let (sound, text) = tail.split_once(' ').unwrap_or((tail, ""));
                let want_sound: u8 = sound.parse().unwrap_or(0);
                // the transcript calls its players p0..p7; ours need names
                // that stay unique across re-seats, so put ours back in.
                // Highest index first, so p1 cannot eat the start of p10.
                let mut text = text.to_owned();
                for i in (0..hh.players.len()).rev() {
                    let name = hh
                        .zone
                        .players
                        .get(hh.players[i])
                        .map(|x| x.name.clone())
                        .unwrap_or_default();
                    text = text.replace(&format!("p{i}"), &name);
                }
                let text = text.as_str();
                let lines = if who == "arena" {
                    &arena_said
                } else {
                    let i: usize = who.parse().expect("player index");
                    said.get(i).expect("player said")
                };
                let at = chat_at.entry(who.to_owned()).or_insert(0);
                // an arena line also lands in each player's own list, so the
                // per-player cursors and the arena cursor both have to skip it
                let found = lines[*at..]
                    .iter()
                    .position(|(s, t)| *s == want_sound && t == text);
                assert!(
                    found.is_some(),
                    "{label}: {who} never heard {text:?}, only {lines:?}"
                );
                *at += found.unwrap_or(0) + 1;
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(scenarios >= 20, "only {scenarios} scenarios replayed");
    assert!(checked > 120, "only {checked} records checked");
}
