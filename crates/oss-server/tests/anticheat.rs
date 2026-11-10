//! The anti-cheat module: off by default, log-only unless told otherwise,
//! no findings for a client playing at the limits of its ship, and the
//! physics checks against oss-sim's validation.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::position::{C2SPosition, WeaponInfo, weapon_type};
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::anticheat::{CheatSuspected, Check, trust};
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

/// A Warbird roughly as in the ASSS sample zone.
const SHIP: &str = "[Warbird]\nBulletFireDelay = 50\nMultiFireDelay = 50\nMaxGuns = 2\nMaxBombs = 1\n\
InitialEnergy = 1500\nMaximumEnergy = 1700\nInitialRecharge = 400\nMaximumRecharge = 1150\n\
InitialSpeed = 2010\nMaximumSpeed = 3250\nInitialRotation = 200\nMaximumRotation = 300\n\
InitialThrust = 16\nMaximumThrust = 20\nBulletFireEnergy = 20\n[Misc]\nBounceFactor = 22\n";

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    seen: Arc<Mutex<Vec<Check>>>,
}

impl H {
    fn new(ac: &str) -> Self {
        Self::with(ac, "")
    }

    fn with(ac: &str, extra: &str) -> Self {
        Self::with_map(ac, extra, None)
    }

    /// `map` is a raw (uncompressed) `.lvl` tile list.
    fn with_map(ac: &str, extra: &str, map: Option<Vec<u8>>) -> Self {
        let arena = format!("{SHIP}{extra}[AntiCheat]\n{ac}");
        let mut src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_trust = 1\nprivcmd_trust = 1\n".to_owned(),
            ),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", arena),
        ]);
        if let Some(m) = map {
            src.insert("maps/test.lvl", m);
        }
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        zone.on::<CheatSuspected>(Owner(999), Scope::Global, move |_, ev| {
            s.lock().expect("lock").push(ev.check);
        });
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            seen,
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

    fn player(&mut self, name: &str) -> PlayerId {
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
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        self.zone.handle_packet(p, &[c2s::kind::SET_SHIP, 0]);
        self.tick(1);
        p
    }

    /// A full position packet.
    #[allow(clippy::too_many_arguments)]
    fn send_pos(
        &mut self,
        p: PlayerId,
        time: u32,
        x: i16,
        y: i16,
        xs: i16,
        energy: i16,
        kind: u8,
        level: u8,
    ) {
        let mut w = WeaponInfo::default();
        w.set_kind(kind);
        w.set_level(level);
        let mut buf = Vec::new();
        C2SPosition {
            time,
            x,
            y,
            x_speed: xs,
            energy,
            weapon: w,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.lo.collect(&mut self.zone);
    }

    /// Standing still at (8000, 8000) with ordinary energy, firing `kind`.
    fn pos(&mut self, p: PlayerId, time: u32, kind: u8, level: u8) {
        self.send_pos(p, time, 8000, 8000, 0, 1000, kind, level);
    }

    /// A position packet with the client's flash bit set: a warp, a portal
    /// or an uncloak, as the client tells it.
    fn flash_pos(&mut self, p: PlayerId, time: u32, x: i16, y: i16, energy: i16) {
        let mut buf = Vec::new();
        C2SPosition {
            time,
            x,
            y,
            energy,
            status: oss_proto::position::status::FLASH,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.lo.collect(&mut self.zone);
    }

    fn seen(&self) -> Vec<Check> {
        self.seen.lock().expect("lock").clone()
    }

    fn ship(&self, p: PlayerId) -> u8 {
        self.zone.players.get(p).map_or(99, |x| x.ship.0)
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
}

#[test]
fn off_by_default() {
    let mut h = H::new("");
    let p = h.player("alice");
    for i in 0..10 {
        h.pos(p, 1000 + i * 5, weapon_type::BULLET, 3);
    }
    h.send_pos(p, 1100, 16000, 16000, 30000, 30000, 0, 0);
    assert!(h.seen().is_empty());
    assert_eq!(trust(&h.zone, p), 100);
}

#[test]
fn honest_play_at_the_limits() {
    let mut h = H::new("Enabled = 1\nAction = 2\n");
    let p = h.player("alice");
    // a minute of flying flat out and firing at the ship's limits: top speed
    // (3250 px/10 s), highest gun level, shots exactly BulletFireDelay apart
    let start = h.t as u32;
    let mut x = 2000i32;
    for i in 0..120u32 {
        h.tick(50);
        x += 3250 * 50 / 1000;
        if x > 15000 {
            // wrap round: a ?warpto, which the server excuses
            x = 2000;
            oss_server::modules::anticheat::excuse(
                &mut h.zone,
                p,
                oss_server::modules::anticheat::Excuse::Jump,
            );
        }
        let t = start + i * 50;
        h.send_pos(p, t, x as i16, 8000, 3250, 1000, weapon_type::BULLET, 1);
    }
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    assert_eq!(h.ship(p), 0);
}

#[test]
fn log_only_by_default_when_enabled() {
    let mut h = H::new("Enabled = 1\n");
    let p = h.player("alice");
    h.pos(p, 1000, weapon_type::BULLET, 0);
    h.pos(p, 1010, weapon_type::BULLET, 0);
    assert_eq!(h.seen(), vec![Check::FireRate]);
    assert_eq!(trust(&h.zone, p), 90);
    assert_eq!(h.ship(p), 0, "Action = 0 only logs");
    // a check counts once per cooldown
    h.pos(p, 1020, weapon_type::BULLET, 0);
    assert_eq!(h.seen().len(), 1);
    // over-level guns
    h.pos(p, 1200, weapon_type::BULLET, 2);
    assert_eq!(h.seen(), vec![Check::FireRate, Check::WeaponLevel]);
}

#[test]
fn duplicate_packets_are_not_shots() {
    let mut h = H::new("Enabled = 1\n");
    let p = h.player("alice");
    h.pos(p, 1000, weapon_type::BULLET, 0);
    h.pos(p, 1000, weapon_type::BULLET, 0);
    assert!(h.seen().is_empty(), "{:?}", h.seen());
}

#[test]
fn ball_carrier_gets_the_flagger_upgrade() {
    let soccer = "[Soccer]\nBallCount = 1\nSpawnX = 512\nSpawnY = 512\nSpawnRadius = 1\nUseFlagger = 1\n\
                  [Flag]\nFlaggerGunUpgrade = 1\n";
    let mut h = H::with("Enabled = 1\n", soccer);
    let p = h.player("alice");
    let arena = h.zone.players.get(p).and_then(|x| x.arena).expect("arena");
    let ball = oss_server::modules::balls::balls(&h.zone, arena)[0];
    let mut buf = Vec::new();
    c2s::PickupBall {
        ball_id: 0,
        time: ball.time,
    }
    .encode(&mut buf);
    h.zone.handle_packet(p, &buf);
    assert_eq!(
        oss_server::modules::balls::balls(&h.zone, arena)[0].carrier,
        Some(p)
    );
    // level 3 guns on a MaxGuns = 2 ship: legitimate while carrying the ball
    h.pos(p, 1000, weapon_type::BULLET, 2);
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    // without UseFlagger the same shot is a finding
    let mut h2 = H::with(
        "Enabled = 1\n",
        "[Soccer]\nBallCount = 1\nSpawnX = 512\nSpawnY = 512\nSpawnRadius = 1\nUseFlagger = 0\n",
    );
    let q = h2.player("bob");
    let a2 = h2.zone.players.get(q).and_then(|x| x.arena).expect("arena");
    let ball = oss_server::modules::balls::balls(&h2.zone, a2)[0];
    let mut buf = Vec::new();
    c2s::PickupBall {
        ball_id: 0,
        time: ball.time,
    }
    .encode(&mut buf);
    h2.zone.handle_packet(q, &buf);
    h2.pos(q, 1000, weapon_type::BULLET, 2);
    assert_eq!(h2.seen(), vec![Check::WeaponLevel]);
}

#[test]
fn spec_until_trust_recovers() {
    let mut h = H::new(
        "Enabled = 1\nAction = 1\nSpecTrust = 80\nWeaponLevelWeight = 30\nRecoverPerMinute = 60\n",
    );
    let p = h.player("alice");
    h.lo.take(p);
    h.pos(p, 1000, weapon_type::BULLET, 3);
    assert_eq!(trust(&h.zone, p), 70);
    assert_eq!(h.ship(p), 8);
    assert!(
        h.texts(p).iter().any(|t| t
            == "You have been moved to spectator mode by the anti-cheat checks (weapon level).")
    );
    // kept out of ships meanwhile
    h.zone.handle_packet(p, &[c2s::kind::SET_SHIP, 1]);
    h.lo.collect(&mut h.zone);
    assert_eq!(h.ship(p), 8);
    // ?trust shows the finding
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PUBLIC,
        sound: 0,
        pid: 0,
        text: b"?trust".to_vec(),
    })
    .encode(&mut buf);
    h.texts(p);
    h.zone.handle_packet(p, &buf);
    h.lo.collect(&mut h.zone);
    let out = h.texts(p);
    assert!(
        out.iter().any(|t| t == "alice: trust 70 (kept in spec)"),
        "{out:?}"
    );
    assert!(out.iter().any(|t| t.contains("weapon level")), "{out:?}");
    // 1 point a second: back above 80 after about 10 s
    h.tick(1200);
    assert!(trust(&h.zone, p) >= 80);
    h.zone.handle_packet(p, &[c2s::kind::SET_SHIP, 1]);
    h.lo.collect(&mut h.zone);
    assert_eq!(h.ship(p), 1);
}

#[test]
fn fast_client_clock() {
    let mut h = H::new(
        "Enabled = 1\nAction = 1\nClockWindow = 10\nClockDrift = 5\nClockWeight = 70\nMotionChecks = 0\n",
    );
    let p = h.player("alice");
    let fast = h.player("bob");
    h.lo.take(fast);
    let start = h.t as u32;
    // 25 s of positions; bob's clock runs 20% fast: two windows in a row
    for i in 0..251u32 {
        h.tick(10);
        h.pos(p, start + i * 10, weapon_type::NONE, 0);
        h.pos(fast, start + i * 12, weapon_type::NONE, 0);
    }
    assert_eq!(h.seen(), vec![Check::Clock]);
    assert_eq!(trust(&h.zone, p), 100);
    assert_eq!(trust(&h.zone, fast), 30);
    // the player is told it's their clock
    assert!(
        h.texts(fast)
            .iter()
            .any(|t| t.starts_with("Your computer's clock is running fast (20.0% ahead")),
    );
}

#[test]
fn teleports_speeds_and_energy() {
    let mut h = H::new("Enabled = 1\nCooldown = 0\n");
    let p = h.player("alice");
    let t0 = h.t as u32;
    h.tick(10);
    h.send_pos(p, t0, 4000, 4000, 0, 1000, 0, 0);
    // 5000 pixels in a tenth of a second
    h.tick(10);
    h.send_pos(p, t0 + 10, 9000, 4000, 0, 1000, 0, 0);
    assert_eq!(h.seen(), vec![Check::Teleport]);
    // a reported speed ten times the ship's top speed
    h.tick(10);
    h.send_pos(p, t0 + 20, 9000, 4000, 30000, 1000, 0, 0);
    assert_eq!(h.seen().last(), Some(&Check::Speed));
    // more energy than the ship can hold
    h.tick(10);
    h.send_pos(p, t0 + 30, 9000, 4000, 0, 5000, 0, 0);
    assert_eq!(h.seen().last(), Some(&Check::Energy));
    let n = h.seen().len();
    // a jump the server caused is fine: ?warpto, a death
    oss_server::modules::anticheat::excuse(
        &mut h.zone,
        p,
        oss_server::modules::anticheat::Excuse::Jump,
    );
    h.tick(10);
    h.send_pos(p, t0 + 40, 2000, 12000, 0, 1000, 0, 0);
    assert_eq!(h.seen().len(), n, "{:?}", h.seen());
    // so is a client claiming lots of time passed while the server saw little
    // (the elapsed time is capped by the server's clock): that's a teleport
    h.tick(10);
    h.send_pos(p, t0 + 10_040, 12000, 12000, 0, 1000, 0, 0);
    assert_eq!(h.seen().last(), Some(&Check::Teleport));
}

#[test]
fn a_short_burst_of_fast_clock_is_caught() {
    // The clock check averages over 20 s, so a burst this short is invisible
    // to it; the motion bounds have to be what sees it.
    let mut h = H::new("Enabled = 1\nCooldown = 0\n");
    let p = h.player("alice");
    let (mut t, mut x) = (h.t as u32, 2000i32);
    // a second of honest flight at the ship's top speed
    for _ in 0..10 {
        h.tick(10);
        t += 10;
        x += 32;
        h.send_pos(p, t, x as i16, 8000, 3250, 1000, 0, 0);
    }
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    // the clock jumps to four times speed: each packet claims 40 ticks where
    // the server saw 10, and the ship covers what 40 ticks would allow. The
    // first second or so is paid for out of the budget (a lag spike looks
    // like this); after that the bounds go by the server's clock.
    for i in 0..25 {
        h.tick(10);
        t += 40;
        x += 130;
        h.send_pos(p, t, x as i16, 8000, 3250, 1000, 0, 0);
        if i == 5 {
            assert!(h.seen().is_empty(), "{:?}", h.seen());
        }
    }
    assert!(
        h.seen().contains(&Check::Teleport),
        "a 4x clock burst went unnoticed: {:?}",
        h.seen()
    );
}

#[test]
fn packets_bunched_after_a_lag_spike_are_not_findings() {
    // The same mechanism must not punish a backlog: three seconds of
    // positions delivered at once, each claiming its honest 10 ticks.
    let mut h = H::new("Enabled = 1\nCooldown = 0\n");
    let p = h.player("alice");
    let (mut t, mut x) = (h.t as u32, 2000i32);
    h.tick(10);
    h.send_pos(p, t, x as i16, 8000, 0, 1000, 0, 0);
    for _ in 0..30 {
        t += 10;
        x += 20;
        h.send_pos(p, t, x as i16, 8000, 0, 1000, 0, 0);
    }
    assert!(h.seen().is_empty(), "{:?}", h.seen());
}

#[test]
fn item_counts() {
    let items = "[Warbird]\nInitialRepel = 2\nRepelMax = 3\nThorMax = 0\n";
    let mut h = H::with("Enabled = 1\nCooldown = 0\n", items);
    let p = h.player("alice");
    // a thor on a ship that can never hold one
    h.pos(p, 1000, weapon_type::THOR, 0);
    assert_eq!(h.seen(), vec![Check::Item]);
    // two repels to start with: the third has nothing behind it
    h.pos(p, 1100, weapon_type::REPEL, 0);
    h.pos(p, 1200, weapon_type::REPEL, 0);
    assert_eq!(h.seen().len(), 1);
    h.pos(p, 1300, weapon_type::REPEL, 0);
    assert_eq!(h.seen(), vec![Check::Item, Check::Item]);
    // a repel prize (21) gives one more
    oss_server::modules::anticheat::prize_taken(&mut h.zone, p, 21);
    h.pos(p, 1400, weapon_type::REPEL, 0);
    assert_eq!(h.seen().len(), 2);
}

#[test]
fn random_spawn_prizes_make_items_unknown() {
    // InitialBounty hands out random prizes the server can't replay, so any
    // item up to its maximum may be held
    let items =
        "[Warbird]\nInitialRepel = 0\nRepelMax = 3\nInitialBounty = 10\n[PrizeWeight]\nRepel = 1\n";
    let mut h = H::with("Enabled = 1\nCooldown = 0\n", items);
    let p = h.player("alice");
    for i in 0..3 {
        h.pos(p, 1000 + i * 100, weapon_type::REPEL, 0);
    }
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    h.pos(p, 1400, weapon_type::REPEL, 0);
    assert_eq!(h.seen(), vec![Check::Item]);
}

#[test]
fn flash_bit_alone_explains_nothing() {
    // the spawn area is tile (300, 300), radius 20: pixel (4800, 4800)
    let spawn = "[Warbird]\nPortalMax = 0\nInitialPortal = 0\n\
                 [Spawn]\nTeam0-X = 300\nTeam0-Y = 300\nTeam0-Radius = 20\n";
    let mut h = H::with("Enabled = 1\nCooldown = 0\n", spawn);
    let p = h.player("alice");
    let t0 = h.t as u32;
    let flash_jump = |h: &mut H, t: u32, x: i16, y: i16, e_before: i16, e_after: i16| {
        // the ship starts each jump where the server put it, charged up
        oss_server::modules::anticheat::excuse(
            &mut h.zone,
            p,
            oss_server::modules::anticheat::Excuse::Both,
        );
        h.tick(10);
        h.send_pos(p, t, 4000, 4000, 0, e_before, 0, 0);
        h.tick(10);
        h.flash_pos(p, t + 10, x, y, e_after);
    };
    // pinning the flash bit doesn't excuse a jump
    flash_jump(&mut h, t0, 9000, 4000, 1000, 1000);
    assert_eq!(h.seen(), vec![Check::Teleport]);
    // nor does the energy signature of a warp on its own: the client picks
    // both numbers, and this one came out nowhere near a spawn
    flash_jump(&mut h, t0 + 100, 9000, 4000, 1700, 1);
    assert_eq!(h.seen(), vec![Check::Teleport, Check::Teleport]);
    // a real warp to spawn: full energy before, drained after, and the ship
    // comes out in the arena's spawn area
    flash_jump(&mut h, t0 + 200, 4800, 4800, 1700, 1);
    assert_eq!(h.seen().len(), 2, "{:?}", h.seen());
}

/// One flash-bit hop across the wall at tile 300: the server puts the ship
/// back on the left of it (a jump the server knows about and excuses), then
/// the client claims the crossing with the flash bit alone.
fn flash_hop(h: &mut H, p: PlayerId, t: u32) {
    oss_server::modules::anticheat::excuse(
        &mut h.zone,
        p,
        oss_server::modules::anticheat::Excuse::Jump,
    );
    h.tick(10);
    h.send_pos(p, t, 4796, 4800, 0, 1000, 0, 0);
    h.tick(10);
    h.flash_pos(p, t + 10, 4820, 4800, 1000);
}

#[test]
fn flash_jumps_through_a_wall_cost_a_portal() {
    // a wall column the ship can't get round in the distance it may travel
    let mut map = Vec::new();
    for y in 295..=305u32 {
        map.extend_from_slice(&(300u32 | (y << 12) | (1u32 << 24)).to_le_bytes());
    }
    let mut h = H::with_map(
        "Enabled = 1\nCooldown = 0\n",
        "[General]\nMap = test.lvl\n[Warbird]\nPortalMax = 2\nInitialPortal = 2\n",
        Some(map),
    );
    let p = h.player("alice");
    let t0 = h.t as u32;
    // two portals, two hops the server can't disprove
    flash_hop(&mut h, p, t0);
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    flash_hop(&mut h, p, t0 + 100);
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    // the third has nothing behind it: this is the wall hack
    flash_hop(&mut h, p, t0 + 200);
    assert_eq!(h.seen(), vec![Check::Wall]);
    flash_hop(&mut h, p, t0 + 300);
    assert_eq!(h.seen(), vec![Check::Wall, Check::Wall]);
    // a portal prize (28) buys one more hop, and only one
    oss_server::modules::anticheat::prize_taken(&mut h.zone, p, 28);
    flash_hop(&mut h, p, t0 + 400);
    assert_eq!(h.seen().len(), 2, "{:?}", h.seen());
    flash_hop(&mut h, p, t0 + 500);
    assert_eq!(h.seen().len(), 3, "{:?}", h.seen());
}

#[test]
fn a_ship_with_no_portals_never_gets_a_free_wall_crossing() {
    let mut map = Vec::new();
    for y in 295..=305u32 {
        map.extend_from_slice(&(300u32 | (y << 12) | (1u32 << 24)).to_le_bytes());
    }
    let mut h = H::with_map(
        "Enabled = 1\nCooldown = 0\n",
        "[General]\nMap = test.lvl\n[Warbird]\nPortalMax = 0\nInitialPortal = 0\n",
        Some(map),
    );
    let p = h.player("alice");
    let t0 = h.t as u32;
    flash_hop(&mut h, p, t0);
    assert_eq!(h.seen(), vec![Check::Wall]);
}

#[test]
fn endless_energy_while_firing() {
    // shots dear enough that recharge can't keep up: 300 energy each, two a
    // second, against 115 a second of recharge
    let pricey = "[Warbird]\nBulletFireEnergy = 300\n";
    for (honest, want) in [(true, 0), (false, 1)] {
        let mut h = H::with("Enabled = 1\nCooldown = 0\n", pricey);
        let p = h.player("alice");
        let t0 = h.t as u32;
        let mut energy = 1700i32;
        for i in 0..8u32 {
            h.tick(50);
            if honest {
                energy = (energy - 300 + 57).max(0);
            }
            h.send_pos(
                p,
                t0 + i * 50,
                8000,
                8000,
                0,
                energy as i16,
                weapon_type::BULLET,
                0,
            );
        }
        let n = h.seen().iter().filter(|c| **c == Check::Energy).count();
        assert_eq!(n.min(1), want, "honest: {honest}, {:?}", h.seen());
    }
}

#[test]
fn one_impossible_finding_is_forgiven_and_two_are_not() {
    // The impossible checks are weighted 50, so the first finding leaves
    // trust at 50 -- above SpecTrust 40 -- and the second leaves 0, below
    // KickTrust 10. Under Action = 2 that second one is a kick, not a spec.
    let mut map = Vec::new();
    for y in 295..=305u32 {
        map.extend_from_slice(&(300u32 | (y << 12) | (1u32 << 24)).to_le_bytes());
    }
    let mut h = H::with_map(
        "Enabled = 1\nAction = 2\nCooldown = 0\n",
        "[General]\nMap = test.lvl\n[Warbird]\nPortalMax = 0\nInitialPortal = 0\n",
        Some(map),
    );
    let p = h.player("alice");
    let t0 = h.t as u32;

    flash_hop(&mut h, p, t0);
    assert_eq!(h.seen(), vec![Check::Wall]);
    assert_eq!(trust(&h.zone, p), 50);
    assert_eq!(h.ship(p), 0, "one finding does not cost the ship");
    assert!(h.lo.take_disconnects().is_empty());

    flash_hop(&mut h, p, t0 + 100);
    assert_eq!(h.seen(), vec![Check::Wall, Check::Wall]);
    assert_eq!(trust(&h.zone, p), 0);
    assert_eq!(h.ship(p), 8, "specced");
    // the kick runs through the lifecycle rather than dropping the socket
    // there and then, so let it play out
    h.tick(50);
    assert_eq!(h.lo.take_disconnects().len(), 1, "and kicked");
}

/// Being specced by the server is not a teleport.
///
/// The question came from the client thread's lag write-up: a false spec
/// moves the player's ship, and if that move were judged as motion an
/// honest client would earn a Teleport finding at the exact moment it
/// happened -- and Teleport is one of the checks weighted 50, so two would
/// be a kick. The exemption is stronger than an excuse: a ship or freq
/// change clears the player's previous position sample, so there is no
/// pair to compare and the first packet afterwards just becomes the new
/// baseline. A spectator is not motion-checked at all on the way through.
#[test]
fn a_server_spec_and_the_ship_after_it_are_not_a_teleport() {
    let mut h = H::with(
        "Enabled = 1\nAction = 2\nCooldown = 0\n",
        "[General]\nMap = test.lvl\n",
    );
    let p = h.player("alice");
    let t0 = h.t as u32;
    h.send_pos(p, t0, 2000, 8000, 0, 1000, 0, 0);
    h.tick(10);
    assert!(h.seen().is_empty(), "{:?}", h.seen());

    // lagaction and anticheat both spec through set_ship_and_freq
    let sf = 8025;
    oss_server::modules::game::set_ship_and_freq(
        &mut h.zone,
        p,
        oss_server::player::Ship::SPEC,
        sf,
    );
    h.tick(10);

    // back into a ship, spawning right across the map from where they were
    oss_server::modules::game::set_ship_and_freq(&mut h.zone, p, oss_server::player::Ship(0), 0);
    h.tick(10);
    h.send_pos(p, h.t as u32, 15000, 1000, 0, 1000, 0, 0);
    h.tick(10);
    assert!(
        h.seen().is_empty(),
        "a server-initiated spec should leave no finding: {:?}",
        h.seen()
    );
}
