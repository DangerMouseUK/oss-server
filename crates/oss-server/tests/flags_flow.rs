//! Flag games over the loopback transport: warzone (spawn, pickup, drop,
//! transfer on kill, win with points_flag reward) and turf (static flags
//! claimed by touch).

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::position::C2SPosition;
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::flagcore::{self, FlagState};
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

fn tile_list(tiles: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut b = Vec::new();
    for &(x, y, t) in tiles {
        b.extend_from_slice(&(x | (y << 12) | (t << 24)).to_le_bytes());
    }
    b
}

impl H {
    fn new(arena_conf: &str, map: Option<Vec<u8>>) -> Self {
        let mut src = MemSource::new([
            ("conf/global.conf", ""),
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_flaginfo = 1\ncmd_neutflag = 1\ncmd_moveflag = 1\n",
            ),
            ("conf/staff.conf", ""),
            ("arenas/(default)/arena.conf", arena_conf),
        ]);
        if let Some(m) = map {
            src.insert("maps/test.lvl", m);
        }
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        // fm_normal picks at random between equally populated teams, so the
        // tests fix the seed to keep who lands where repeatable
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

    fn take(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }

    fn player(&mut self, name: &str, ship: u8) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let login = Login {
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
        };
        let mut buf = Vec::new();
        login.encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        if ship < 8 {
            self.send(p, &[c2s::kind::SET_SHIP, ship]);
        }
        p
    }

    fn at(&mut self, p: PlayerId, x_tile: i16, y_tile: i16) {
        let pos = C2SPosition {
            time: self.t as u32 + 1,
            x: x_tile * 16,
            y: y_tile * 16,
            ..Default::default()
        };
        let mut buf = Vec::new();
        pos.encode(&mut buf);
        self.send(p, &buf);
    }

    fn touch(&mut self, p: PlayerId, fid: i16) {
        let mut buf = Vec::new();
        c2s::PickupFlag { flag_id: fid }.encode(&mut buf);
        self.send(p, &buf);
    }

    fn arena(&self, p: PlayerId) -> oss_server::arena::ArenaId {
        self.zone
            .players
            .get(p)
            .and_then(|p| p.arena)
            .expect("arena")
    }

    fn carried(&self, p: PlayerId) -> i16 {
        self.zone.players.get(p).map_or(-1, |p| p.flags_carried)
    }

    /// Types a command in public chat and returns what came back.
    fn cmd(&mut self, p: PlayerId, text: &str) -> Vec<String> {
        self.take(p);
        let mut buf = Vec::new();
        c2s::C2SChat(s2c::ChatBody {
            kind: oss_proto::common::chat_type::PUBLIC,
            sound: 0,
            pid: 0,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.send(p, &buf);
        of(&self.take(p), s2c::kind::CHAT)
            .iter()
            .map(|c| {
                String::from_utf8_lossy(&s2c::S2CChat::decode(c).expect("chat").0.text).into_owned()
            })
            .collect()
    }
}

fn of(pkts: &[Vec<u8>], kind: u8) -> Vec<Vec<u8>> {
    pkts.iter().filter(|d| d[0] == kind).cloned().collect()
}

#[test]
fn warzone_game() {
    let conf = "[Modules]\nAttachModules = fg_wz points_flag\n\
        [Flag]\nFlagCount = 2\nSpawnX = 512\nSpawnY = 512\nSpawnRadius = 5\nDropRadius = 1\nWinDelay = 50\n\
        FlagReward = 5000\n[Kill]\nJackpotBountyPercent = 1000\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, None);
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    assert_eq!(h.zone.players.get(a).map(|p| p.freq), Some(0));
    assert_eq!(h.zone.players.get(b).map(|p| p.freq), Some(1));
    h.take(a);
    let arena = h.arena(a);

    // flags spawn within the spawn radius and are announced
    h.tick(500);
    let locs = of(&h.take(a), s2c::kind::FLAG_LOCATION);
    assert_eq!(locs.len(), 2);
    for l in &locs {
        let l = s2c::FlagLocation::decode(l).expect("0x12");
        assert!(
            (l.x as i32 - 512).abs() <= 6 && (l.y as i32 - 512).abs() <= 6,
            "{l:?}"
        );
        assert_eq!(l.freq, 0xFFFF);
    }

    // a picks up flag 0
    h.at(a, 300, 300);
    h.touch(a, 0);
    let pick = of(&h.take(b), s2c::kind::FLAG_PICKUP);
    let pick = s2c::FlagPickup::decode(&pick[0]).expect("0x13");
    assert_eq!((pick.flag_id, pick.pid), (0, a.0 as i16));
    assert_eq!(h.carried(a), 1);
    // touching a carried flag is refused
    h.touch(b, 0);
    assert!(of(&h.take(b), s2c::kind::FLAG_PICKUP).is_empty());

    // a drops: the flag lands next to a, owned by freq 0
    h.send(a, &[c2s::kind::DROP_FLAGS]);
    let got = h.take(b);
    assert_eq!(of(&got, s2c::kind::FLAG_DROP).len(), 1);
    let l = s2c::FlagLocation::decode(&of(&got, s2c::kind::FLAG_LOCATION)[0]).expect("0x12");
    assert_eq!(l.freq, 0);
    assert!(
        (l.x as i32 - 300).abs() <= 2 && (l.y as i32 - 300).abs() <= 2,
        "{l:?}"
    );
    assert_eq!(h.carried(a), 0);

    // a picks it up again; b kills a with bounty 40 and takes it
    h.touch(a, 0);
    let mut buf = Vec::new();
    c2s::Death {
        killer: b.0 as i16,
        bounty: 40,
    }
    .encode(&mut buf);
    h.send(a, &buf);
    let kill = s2c::Kill::decode(&of(&h.take(b), s2c::kind::KILL)[0]).expect("0x06");
    assert_eq!(kill.flags, 1);
    assert_eq!((h.carried(a), h.carried(b)), (0, 1));
    let f = flagcore::get_flag(&h.zone, arena, 0).expect("flag");
    assert_eq!(
        (f.state, f.carrier, f.freq),
        (FlagState::Carried, Some(b), 1)
    );

    // b drops it, then takes and drops flag 1: freq 1 owns both and wins
    h.at(b, 600, 600);
    h.send(b, &[c2s::kind::DROP_FLAGS]);
    h.touch(b, 1);
    h.send(b, &[c2s::kind::DROP_FLAGS]);
    h.take(a);
    h.tick(60);
    let win = of(&h.take(a), s2c::kind::FLAG_VICTORY);
    let win = s2c::FlagVictory::decode(&win[0]).expect("0x14");
    // 2 players: 2*2*5000/1000 = 20, plus the jackpot (100% of bounty 40)
    assert_eq!((win.freq, win.points), (1, 60));
    assert_eq!(h.zone.players.get(b).map(|p| p.score.flag_points), Some(60));
    assert!(flagcore::get_flag(&h.zone, arena, 0).is_some_and(|f| f.state == FlagState::None));
    // a new game starts: flags respawn
    h.tick(500);
    assert_eq!(of(&h.take(a), s2c::kind::FLAG_LOCATION).len(), 2);
}

#[test]
fn turf_game() {
    let map = tile_list(&[(100, 100, 170), (200, 200, 170), (300, 300, 1)]);
    let conf = "[General]\nMap = test.lvl\n[Modules]\nAttachModules = fg_turf\n";
    let mut h = H::new(conf, Some(map));
    let a = h.player("alpha", 0);
    let pkts = h.take(a);
    let turf = of(&pkts, s2c::kind::TURF_FLAGS);
    assert_eq!(
        s2c::TurfFlags::decode(&turf[0]).expect("0x22").owners,
        vec![-1, -1]
    );
    // Flag:CarryFlags is forced to "none" in the settings
    let settings = of(&pkts, s2c::kind::SETTINGS);
    let k = oss_server::clientset::override_key("Flag", "CarryFlags").expect("key");
    let block = oss_server::clientset::SettingsBlock::from_bytes(
        settings[0].as_slice().try_into().expect("len"),
    );
    assert_eq!(block.get(k), 0);

    h.touch(a, 1);
    let pick = of(&h.take(a), s2c::kind::FLAG_PICKUP);
    assert_eq!(s2c::FlagPickup::decode(&pick[0]).expect("0x13").flag_id, 1);
    let arena = h.arena(a);
    assert_eq!(
        flagcore::get_flag(&h.zone, arena, 1).map(|f| f.freq),
        Some(0)
    );
    assert_eq!(flagcore::count_freq_flags(&h.zone, arena, 0), 1);
    // drops make no sense in turf
    h.send(a, &[c2s::kind::DROP_FLAGS]);
    assert!(of(&h.take(a), s2c::kind::FLAG_DROP).is_empty());
    // periodic ownership broadcast
    h.tick(3000);
    let turf = of(&h.take(a), s2c::kind::TURF_FLAGS);
    assert_eq!(
        s2c::TurfFlags::decode(turf.last().expect("0x22"))
            .expect("decode")
            .owners,
        vec![-1, 0]
    );
}

#[test]
fn soccer_balls() {
    let conf = "[Soccer]\nBallCount = 1\nSpawnX = 512\nSpawnY = 512\nSpawnRadius = 3\nSendTime = 100\nGoalDelay = 0\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, None);
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    let arena = h.arena(a);
    let ball = oss_server::modules::balls::balls(&h.zone, arena)[0];
    assert!(
        ((ball.x >> 4) - 512).abs() <= 4 && ((ball.y >> 4) - 512).abs() <= 4,
        "{ball:?}"
    );
    h.take(a);
    h.take(b);

    // positions are broadcast periodically
    h.tick(400);
    let bp = of(&h.take(b), s2c::kind::BALL);
    let bp = s2c::BallPosition::decode(bp.last().expect("0x2E")).expect("decode");
    assert_eq!((bp.pid, bp.time), (-1, ball.time));

    // a stale pickup is refused; a fresh one succeeds
    let pickup = |time: u32| {
        let mut buf = Vec::new();
        c2s::PickupBall { ball_id: 0, time }.encode(&mut buf);
        buf
    };
    h.send(a, &pickup(ball.time.wrapping_add(1)));
    assert!(of(&h.take(b), s2c::kind::BALL).is_empty());
    h.send(a, &pickup(ball.time));
    let bp = s2c::BallPosition::decode(&of(&h.take(b), s2c::kind::BALL)[0]).expect("0x2E");
    assert_eq!((bp.pid, bp.time), (a.0 as i16, 0));
    // b can't take a carried ball
    h.send(b, &pickup(0));
    assert!(of(&h.take(b), s2c::kind::BALL).is_empty());

    // a fires it
    let mut buf = Vec::new();
    c2s::ShootBall {
        ball_id: 0,
        x: 8000,
        y: 8100,
        x_speed: 50,
        y_speed: 0,
        pid: a.0 as i16,
        time: 777,
    }
    .encode(&mut buf);
    h.send(a, &buf);
    let bp = s2c::BallPosition::decode(&of(&h.take(b), s2c::kind::BALL)[0]).expect("0x2E");
    assert_eq!((bp.x, bp.y, bp.time, bp.pid), (8000, 8100, 777, a.0 as i16));

    // only the last shooter can score; the goal respawns the ball
    let goal = {
        let mut buf = Vec::new();
        c2s::Goal {
            ball_id: 0,
            x: 600,
            y: 512,
        }
        .encode(&mut buf);
        buf
    };
    h.send(b, &goal);
    assert!(of(&h.take(b), s2c::kind::BALL).is_empty());
    h.send(a, &goal);
    let bp = s2c::BallPosition::decode(&of(&h.take(b), s2c::kind::BALL)[0]).expect("0x2E");
    assert_eq!(bp.pid, -1);
    assert!(((bp.x >> 4) - 512).abs() <= 4);
    let stats = h
        .zone
        .iface::<dyn oss_server::modules::stats::Stats>()
        .expect("stats");
    use oss_server::modules::stats::stat;
    assert_eq!(stats.get(&h.zone, a, stat::BALL_GOALS), 1);
    assert_eq!(stats.get(&h.zone, a, stat::BALL_CARRIES), 1);

    // a carrier who changes ship drops the ball
    let ball = oss_server::modules::balls::balls(&h.zone, arena)[0];
    h.send(b, &pickup(ball.time));
    h.send(b, &[c2s::kind::SET_SHIP, 8]);
    let ball = oss_server::modules::balls::balls(&h.zone, arena)[0];
    assert_eq!(
        (ball.state, ball.carrier),
        (oss_server::modules::balls::BallState::OnMap, None)
    );
}

#[test]
fn bricks_drop_resend_and_expire() {
    let conf = "[Brick]\nBrickSpan = 5\nBrickTime = 200\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, None);
    let a = h.player("alpha", 0);
    h.take(a);
    let mut buf = Vec::new();
    c2s::DropBrick { x: 300, y: 300 }.encode(&mut buf);
    h.send(a, &buf);
    let got = of(&h.take(a), s2c::kind::BRICK);
    let b = s2c::Bricks::decode(&got[0]).expect("0x21");
    assert_eq!(b.bricks.len(), 1);
    let br = b.bricks[0];
    assert_eq!(br.freq, 0);
    assert!(br.x1 == br.x2 || br.y1 == br.y2);
    // a player arriving now is sent the live brick
    let late = h.player("late", 8);
    let got: Vec<_> = of(&h.take(late), s2c::kind::BRICK)
        .into_iter()
        .filter(|d| d.len() > 1)
        .collect();
    assert_eq!(got.len(), 1);
    // after BrickTime it's gone for newcomers
    h.tick(300);
    let later = h.player("later", 8);
    assert!(
        of(&h.take(later), s2c::kind::BRICK)
            .iter()
            .all(|d| d.len() == 1)
    );
    // specs can't drop bricks
    h.send(later, &buf);
    assert!(of(&h.take(a), s2c::kind::BRICK).is_empty());
}

#[test]
fn soccer_scoring() {
    // WinBy is 1 rather than the default 0 on purpose: with 0 the leader
    // satisfies its own score+WinBy <= score test, so ASSS counts 8 freqs
    // where it needs 7 and an absolute-scoring game never ends at all.
    let conf = "[Modules]\nAttachModules = points_goal\n\
        [Soccer]\nBallCount = 1\nSpawnRadius = 1\nMode = GOAL_LEFTRIGHT\nCapturePoints = -2\nReward = -100\nNewGameDelay = 100\nWinBy = 1\n\
        [Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, None);
    let a = h.player("alpha", 0);
    let _b = h.player("bravo", 1);
    let arena = h.arena(a);
    let score_goal = |h: &mut H| {
        let ball = oss_server::modules::balls::balls(&h.zone, arena)[0];
        let mut buf = Vec::new();
        c2s::PickupBall {
            ball_id: 0,
            time: ball.time,
        }
        .encode(&mut buf);
        h.send(a, &buf);
        buf.clear();
        c2s::ShootBall {
            ball_id: 0,
            x: 9600,
            y: 8000,
            x_speed: 0,
            y_speed: 0,
            pid: a.0 as i16,
            time: 5,
        }
        .encode(&mut buf);
        h.send(a, &buf);
        buf.clear();
        c2s::Goal {
            ball_id: 0,
            x: 600,
            y: 500,
        }
        .encode(&mut buf);
        h.send(a, &buf);
    };
    h.take(a);
    score_goal(&mut h);
    let texts: Vec<String> = of(&h.take(a), s2c::kind::CHAT)
        .iter()
        .map(|c| {
            String::from_utf8_lossy(&s2c::S2CChat::decode(c).expect("chat").0.text).into_owned()
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.starts_with("Team Goal! by alpha")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "SCORE: Evens:1 Odds:0"),
        "{texts:?}"
    );
    score_goal(&mut h);
    let texts: Vec<String> = of(&h.take(a), s2c::kind::CHAT)
        .iter()
        .map(|c| {
            String::from_utf8_lossy(&s2c::S2CChat::decode(c).expect("chat").0.text).into_owned()
        })
        .collect();
    assert!(texts.iter().any(|t| t == "Soccer game over."), "{texts:?}");
    // Reward is paid on each goal and again on winning (as in ASSS)
    assert_eq!(
        h.zone.players.get(a).map(|p| p.score.flag_points),
        Some(300)
    );
    let ball = oss_server::modules::balls::balls(&h.zone, arena)[0];
    assert_eq!(ball.state, oss_server::modules::balls::BallState::Waiting);
}

#[test]
fn turrets_attach_and_kickoff() {
    let mut h = H::new("[Team]\nDesiredTeams = 1\n", None);
    let a = h.player("alpha", 0);
    let c = h.player("charlie", 1);
    let b = h.player("bravo", 8);
    assert_eq!(h.zone.players.get(c).map(|p| p.freq), Some(0));
    h.take(a);
    let attach = |h: &mut H, p: PlayerId, to: i16| {
        let mut buf = Vec::new();
        c2s::AttachTo { pid: to }.encode(&mut buf);
        h.send(p, &buf);
    };
    attach(&mut h, c, a.0 as i16);
    let t = s2c::Turret::decode(&of(&h.take(a), s2c::kind::TURRET)[0]).expect("0x0E");
    assert_eq!((t.pid, t.host), (c.0 as i16, a.0 as i16));
    assert_eq!(h.zone.players.get(c).and_then(|p| p.attached), Some(a));
    // a spectator on another freq is not a valid host; nor is yourself
    attach(&mut h, c, b.0 as i16);
    attach(&mut h, c, c.0 as i16);
    assert!(of(&h.take(a), s2c::kind::TURRET).is_empty());
    // the host shakes turrets off
    h.send(a, &[c2s::kind::TURRET_KICKOFF]);
    let k =
        s2c::TurretKickoff::decode(&of(&h.take(c), s2c::kind::TURRET_KICKOFF)[0]).expect("0x15");
    assert_eq!(k.pid, a.0 as i16);
    attach(&mut h, c, -1);
    assert_eq!(h.zone.players.get(c).and_then(|p| p.attached), None);
}

#[test]
fn king_of_the_hill() {
    let conf = "[Modules]\nAttachModules = koth\n[King]\nMinPlaying = 2\nExpireTime = 1000\nDeathCount = 0\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, None);
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    h.take(a);
    h.take(b);
    // the check timer starts a round: both get a crown
    h.tick(500);
    for p in [a, b] {
        let k = s2c::Koth::decode(&of(&h.take(p), s2c::kind::KOTH)[0]).expect("0x2C");
        assert_eq!((k.action, k.time, k.pid), (1, 1000, -1));
        assert!(oss_server::modules::koth::has_crown(&h.zone, p));
    }
    // b dies once (DeathCount 0): loses the crown; a's timer is reset
    let mut buf = Vec::new();
    c2s::Death {
        killer: a.0 as i16,
        bounty: 0,
    }
    .encode(&mut buf);
    h.send(b, &buf);
    assert!(!oss_server::modules::koth::has_crown(&h.zone, b));
    let to_a: Vec<_> = of(&h.take(a), s2c::kind::KOTH)
        .iter()
        .map(|k| s2c::Koth::decode(k).expect("0x2C"))
        .collect();
    assert!(to_a.iter().any(|k| k.action == 0 && k.pid == b.0 as i16));
    assert!(to_a.iter().any(|k| k.action == 1 && k.pid == -1));
    // after the next check, a is the only king; when a.s crown expires a
    // wins 1000 alone (crowns lost within one check window share the win)
    h.tick(500);
    h.send(a, &[c2s::kind::KOTH_EXPIRED]);
    h.tick(500);
    assert_eq!(
        h.zone.players.get(a).map(|p| p.score.flag_points),
        Some(1000)
    );
}

#[test]
fn periodic_turf_rewards() {
    let map = tile_list(&[(100, 100, 170), (200, 200, 170)]);
    let conf = "[General]\nMap = test.lvl\n[Modules]\nAttachModules = fg_turf points_periodic\n\
        [Periodic]\nRewardDelay = 1000\nRewardPoints = 50\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, Some(map));
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    h.touch(a, 0);
    h.touch(a, 1);
    h.take(b);
    h.tick(1000);
    let r =
        s2c::PeriodicReward::decode(&of(&h.take(b), s2c::kind::PERIODIC_REWARD)[0]).expect("0x23");
    let r: Vec<(i16, i16)> = r.rewards.iter().map(|x| (x.freq, x.points)).collect();
    assert_eq!(r, vec![(0, 100), (1, 0)]);
    assert_eq!(
        h.zone.players.get(a).map(|p| p.score.flag_points),
        Some(100)
    );
}

/// eLVL regions: entering and leaving raise events every
/// `Misc:RegionCheckInterval`, and no-weapons / no-antiwarp regions strip
/// those from relayed positions.
#[test]
fn regions() {
    use oss_formats::{Chunk, Level, Metadata, Region, RegionBitmap, RegionChunk};
    use oss_server::modules::game::RegionChange;
    use oss_server::registry::{Owner, Scope};
    use std::sync::{Arc, Mutex};

    let mut tiles = RegionBitmap::new();
    for x in 100..110 {
        for y in 100..110 {
            tiles.set(x, y, true);
        }
    }
    let mut safe = Region::new("calm", &tiles);
    safe.chunks.push(RegionChunk::NoWeapons);
    safe.chunks.push(RegionChunk::NoAntiwarp);
    let level = Level {
        metadata: Some(Metadata {
            reserved: 0,
            chunks: vec![Chunk::Region(safe)],
        }),
        ..Level::default()
    };
    let conf = "[General]\nMap = test.lvl\n[Misc]\nRegionCheckInterval = 50\n";
    let mut h = H::new(conf, Some(level.to_bytes()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    h.zone
        .on::<RegionChange>(Owner(999), Scope::Global, move |_, ev| {
            s2.lock().expect("lock").push((ev.region, ev.entering));
        });
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    h.at(b, 104, 104);
    h.tick(60);
    h.at(a, 50, 50);
    h.tick(60);
    h.at(a, 105, 105);
    assert_eq!(*seen.lock().expect("lock"), vec![(0, true)]);
    assert_eq!(
        oss_server::modules::game::player_regions(&h.zone, a),
        vec![0]
    );

    // alpha fires with antiwarp on inside the region: bravo sees neither
    h.take(b);
    let mut w = oss_proto::position::WeaponInfo::default();
    w.set_kind(oss_proto::position::weapon_type::BULLET);
    let pos = C2SPosition {
        time: h.t as u32 + 2,
        x: 105 * 16,
        y: 105 * 16,
        status: oss_proto::position::status::ANTIWARP,
        weapon: w,
        ..Default::default()
    };
    let mut buf = Vec::new();
    pos.encode(&mut buf);
    h.send(a, &buf);
    let got = h.take(b);
    assert!(!got.is_empty());
    for d in &got {
        if d[0] == s2c::kind::WEAPON {
            let w = oss_proto::position::S2CWeapon::decode(d).expect("0x05");
            assert_eq!(w.weapon.kind(), 0);
            assert_eq!(w.status & oss_proto::position::status::ANTIWARP, 0);
        } else if d[0] == s2c::kind::POSITION {
            let p = oss_proto::position::S2CPosition::decode(d).expect("0x28");
            assert_eq!(p.status & oss_proto::position::status::ANTIWARP, 0);
        }
    }

    // leaving
    h.tick(60);
    h.at(a, 50, 50);
    assert_eq!(*seen.lock().expect("lock"), vec![(0, true), (0, false)]);
}

#[test]
fn flag_commands() {
    let conf = "[Modules]\nAttachModules = fg_wz\n\
        [Flag]\nFlagCount = 2\nSpawnX = 512\nSpawnY = 512\nSpawnRadius = 5\nDropRadius = 1\n\
        [Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, None);
    let a = h.player("alpha", 0);
    let arena = h.arena(a);
    h.tick(500);

    // ?flaginfo: both flags are on the map near the spawn
    let out = h.cmd(a, "?flaginfo");
    assert_eq!(out.len(), 2, "{out:?}");
    for (i, line) in out.iter().enumerate() {
        // the spawn is random within its radius, so only the shape is fixed
        assert!(
            line.starts_with(&format!("flag {i}: on the map at ")),
            "{line}"
        );
        assert!(line.contains("(5"), "{line}");
        assert!(line.ends_with("owned by freq -1"), "{line}");
    }

    // ?moveflag puts flag 1 somewhere else and gives it to a freq
    h.cmd(a, "?moveflag 1 3 200 300");
    let f = flagcore::get_flag(&h.zone, arena, 1).expect("flag");
    assert_eq!((f.x, f.y, f.freq), (200, 300, 3));
    let out = h.cmd(a, "?flaginfo");
    assert!(
        out[1].starts_with("flag 1: on the map at D6 (200,300), owned by freq 3"),
        "{out:?}"
    );
    // a bad freq changes nothing
    h.cmd(a, "?moveflag 1 99999 100 100");
    let f = flagcore::get_flag(&h.zone, arena, 1).expect("flag");
    assert_eq!((f.x, f.y, f.freq), (200, 300, 3));

    // ?neutflag takes it off the map
    h.cmd(a, "?neutflag 1");
    assert_eq!(
        flagcore::get_flag(&h.zone, arena, 1).map(|f| f.state),
        Some(FlagState::None)
    );
    // ...and a carried flag needs "force"
    h.at(a, 300, 300);
    h.touch(a, 0);
    assert_eq!(h.carried(a), 1);
    let out = h.cmd(a, "?neutflag 0");
    assert!(
        out.iter()
            .any(|t| t == "neutflag: that flag isn't currently on the map"),
        "{out:?}"
    );
    h.cmd(a, "?neutflag 0 force");
    assert_eq!(h.carried(a), 0);

    let out = h.cmd(a, "?neutflag 7");
    assert!(out.iter().any(|t| t == "neutflag: bad flag id"), "{out:?}");
}

/// turf_reward: weighted flags, the standard jackpot, and the minimums.
#[test]
fn turf_reward_weighted_scoring() {
    let map = tile_list(&[(100, 100, 170), (200, 200, 170), (300, 300, 170)]);
    let conf = "[General]\nMap = test.lvl\n\
        [Modules]\nAttachModules = fg_turf turf_reward points_turf_reward\n\
        [TurfReward]\nRewardStyle = TR_STYLE_STANDARD\nTimerInitial = 1500\n\
        TimerInterval = 1500\nMinPlayersTeam = 1\nMinPlayersArena = 2\nMinTeams = 1\n\
        MinFlags = 1\nMinWeights = 1\nRewardModifier = 100\nSetWeights = 3\n\
        Weight0 = 1\nWeight1 = 5\nWeight2 = 10\n\
        [Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, Some(map));
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    assert_eq!(h.zone.players.get(a).map(|p| p.freq), Some(0));
    assert_eq!(h.zone.players.get(b).map(|p| p.freq), Some(1));

    // alpha takes two flags, bravo one
    h.touch(a, 0);
    h.touch(a, 1);
    h.touch(b, 2);
    h.take(a);
    h.take(b);

    // first ding: every flag is worth Weight0, so alpha holds 2 of 3 weights
    h.tick(1600);
    let texts: Vec<String> = of(&h.take(a), s2c::kind::CHAT)
        .iter()
        .map(|c| {
            String::from_utf8_lossy(&s2c::S2CChat::decode(c).expect("chat").0.text).into_owned()
        })
        .collect();
    // jackpot = RewardModifier * players = 200; alpha's share is 2/3 of it
    assert!(
        texts.iter().any(|t| t == "Reward: 133/200"),
        "alpha's share of the jackpot: {texts:?}"
    );
    assert_eq!(
        h.zone.players.get(a).map(|p| p.score.flag_points),
        Some(133)
    );
    assert_eq!(h.zone.players.get(b).map(|p| p.score.flag_points), Some(66));

    // second ding: the flags have been held through one ding, so they are
    // now worth Weight1 each -- the shares are the same, the points are not
    h.take(a);
    h.tick(1600);
    assert_eq!(
        h.zone.players.get(a).map(|p| p.score.flag_points),
        Some(266)
    );
}

/// turf_reward: a flag tagged back inside the recovery window keeps the
/// weight it had, rather than starting again.
#[test]
fn turf_reward_recovers_a_lost_flag() {
    let map = tile_list(&[(100, 100, 170), (200, 200, 170)]);
    let conf = "[General]\nMap = test.lvl\n\
        [Modules]\nAttachModules = fg_turf turf_reward points_turf_reward\n\
        [TurfReward]\nRewardStyle = TR_STYLE_WEIGHTS\nTimerInitial = 1500\n\
        TimerInterval = 1500\nMinPlayersTeam = 1\nMinPlayersArena = 1\nMinTeams = 1\n\
        SetWeights = 4\nWeight0 = 1\nWeight1 = 4\nWeight2 = 9\nWeight3 = 16\n\
        RecoverDings = 1\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, Some(map));
    let a = h.player("alpha", 0);
    let b = h.player("bravo", 1);
    h.touch(a, 0);
    h.touch(a, 1);
    // two dings: alpha's flags are worth Weight2 = 9 each
    h.tick(1600);
    h.tick(1600);
    let before = h.zone.players.get(a).map(|p| p.score.flag_points);
    assert_eq!(before, Some(2 + 8), "1 then 4 a flag");

    // bravo steals flag 0; it starts again at Weight0 for them
    h.touch(b, 0);
    // alpha tags it straight back, inside the recovery window
    h.touch(a, 0);
    h.take(a);
    h.tick(1600);
    // flag 0 came back at its old weight (9), as did flag 1
    assert_eq!(
        h.zone.players.get(a).map(|p| p.score.flag_points),
        Some(10 + 18),
        "both flags paid 9 again, not 1"
    );
    assert_eq!(h.zone.players.get(b).map(|p| p.score.flag_points), Some(0));
}

/// turf_reward: an arena below its minimums pays nobody and says so.
#[test]
fn turf_reward_minimums() {
    let map = tile_list(&[(100, 100, 170)]);
    let conf = "[General]\nMap = test.lvl\n\
        [Modules]\nAttachModules = fg_turf turf_reward points_turf_reward\n\
        [TurfReward]\nRewardStyle = TR_STYLE_STANDARD\nTimerInitial = 1500\n\
        TimerInterval = 1500\nMinPlayersArena = 5\n[Team]\nDesiredTeams = 2\n";
    let mut h = H::new(conf, Some(map));
    let a = h.player("alpha", 0);
    h.touch(a, 0);
    h.take(a);
    h.tick(1600);
    let texts: Vec<String> = of(&h.take(a), s2c::kind::CHAT)
        .iter()
        .map(|c| {
            String::from_utf8_lossy(&s2c::S2CChat::decode(c).expect("chat").0.text).into_owned()
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t == "Reward: 0 (arena minimum requirements not met)"),
        "{texts:?}"
    );
    assert_eq!(h.zone.players.get(a).map(|p| p.score.flag_points), Some(0));
}
