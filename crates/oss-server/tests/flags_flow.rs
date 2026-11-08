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
            ("conf/groupdef.conf", "[default]\n"),
            ("conf/staff.conf", ""),
            ("arenas/(default)/arena.conf", arena_conf),
        ]);
        if let Some(m) = map {
            src.insert("maps/test.lvl", m);
        }
        let zone = ServerBuilder::with_source(src).build_zone().expect("zone");
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
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
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
    let conf = "[Modules]\nAttachModules = points_goal\n\
        [Soccer]\nBallCount = 1\nSpawnRadius = 1\nMode = GOAL_LEFTRIGHT\nCapturePoints = -2\nReward = -100\nNewGameDelay = 100\n\
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
