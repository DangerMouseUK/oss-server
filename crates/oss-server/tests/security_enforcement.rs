//! What a zone with **no key table** does with a security answer that is
//! wrong: whether it notices, whether it acts, and what `SecurityKickoff` and
//! `bypasssecurity` actually change.
//!
//! **This closes a gap that a flight could not close.** On 2026-11-14 a
//! genuine SubSpace 1.34 client answered four consecutive checks in a zone
//! with no `scrty` file and every one was scored correct. That measured the
//! fallback key path from the client's side, and it is the only side a flight
//! can measure: **a real client cannot be asked to answer wrongly.** So the
//! other half -- that a wrong answer on that same path is detected and acted
//! on -- had never been shown at all. `SecurityKickoff` was off for that run,
//! as it was for the run it was set against, and no tableless zone has ever
//! been flown with the kick enabled.
//!
//! Each of the four things the module's doc comment claims is asserted
//! separately here, because they are four separate behaviours and a single
//! "it kicks" assertion would hide three of them:
//!
//! 1. a wrong answer is **scored** a mismatch (`ok = false`), independently of
//!    what is then done about it;
//! 2. with `Security:SecurityKickoff = 1` the player is **kicked**;
//! 3. with it at `0` the player is **not** kicked and the mismatch is **still
//!    logged** -- the distinction the doc comment draws and that nothing else
//!    tests;
//! 4. `bypasssecurity` **exempts** from the kick, while leaving the log alone
//!    (`suppresssecurity`, not this one, is what silences the log).
//!
//! **Every zone here has no `scrty` file**, which each test asserts rather
//! than assumes: the fallback path is the subject, and a test that quietly ran
//! against a key table would be testing the branch that already worked.
//!
//! The wrong answer is the answer to a **different key** -- what a client that
//! had the wrong idea of the key would really send -- rather than a corrupted
//! field. `exe_checksum` stays at zero throughout, as a genuine VIE 1.34
//! client always leaves it, so the mismatch has to be caught by the settings
//! and map checksum arms and cannot be caught by the exe arm that
//! `vie_zero_checksums.rs` already covers.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login, SecurityResponse};
use oss_proto::position::C2SPosition;
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::clientset::SettingsBlock;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::mapdata::ArenaMap;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// The periodic check goes out at tick 2500 and every 6000 after it.
const INTERVAL: u64 = 6000;
/// Ticks a client has to answer before it is kicked for silence.
const ANSWER_TIME: u64 = 1500;

/// The capability that exempts a player from a security kick.
const BYPASS: &str = "bypasssecurity";

// ---------------------------------------------------------------- log capture

/// One `tracing` event, reduced to what these tests ask about.
#[derive(Clone, Debug)]
struct Line {
    target: String,
    message: String,
    fields: Vec<(String, String)>,
}

impl Line {
    /// The value of a structured field, as it was formatted.
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Rec {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for Rec {
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        if f.name() == "message" {
            self.message = format!("{v:?}");
        } else {
            self.fields.push((f.name().to_owned(), format!("{v:?}")));
        }
    }
}

/// A `tracing` layer that keeps every event instead of printing it.
struct Capture(Arc<Mutex<Vec<Line>>>);

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, ev: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut r = Rec::default();
        ev.record(&mut r);
        if let Ok(mut sink) = self.0.lock() {
            sink.push(Line {
                target: ev.metadata().target().to_owned(),
                message: r.message,
                fields: r.fields,
            });
        }
    }
}

/// Runs `f` with every `tracing` event captured, and hands back the log.
///
/// The subscriber is thread-local (`with_default`), so tests that run side by
/// side in one binary do not collect each other's lines.
fn with_logs<T>(f: impl FnOnce() -> T) -> (T, Vec<Line>) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::registry().with(Capture(Arc::clone(&sink)));
    let out = tracing::subscriber::with_default(sub, f);
    let lines = sink.lock().map(|g| g.clone()).unwrap_or_default();
    (out, lines)
}

/// Every `security` line the run produced, in order.
fn security_lines(log: &[Line]) -> Vec<&Line> {
    log.iter().filter(|l| l.target == "security").collect()
}

/// Whether the run logged `what` against the security module.
fn logged(log: &[Line], what: &str) -> bool {
    security_lines(log).iter().any(|l| l.message.contains(what))
}

/// How the server scored each answer it received, oldest first.
///
/// Reads the `ok` field off the module's own `security response` line, so the
/// test asks the server what it concluded rather than inferring it from what
/// the server then did.
fn scores(log: &[Line]) -> Vec<bool> {
    security_lines(log)
        .iter()
        .filter(|l| l.message == "security response")
        .filter_map(|l| l.field("ok"))
        .map(|v| v == "true")
        .collect()
}

// -------------------------------------------------------------------- the rig

/// A map with a border and a blob, so the map checksum a client has to return
/// is something other than the empty grid's.
fn synthetic_map() -> Vec<u8> {
    let mut out = Vec::new();
    let mut put =
        |x: u32, y: u32, t: u32| out.extend_from_slice(&(x | (y << 12) | (t << 24)).to_le_bytes());
    for i in 0..1024 {
        put(i, 0, 1);
        put(i, 1023, 1);
        put(0, i, 1);
        put(1023, i, 1);
    }
    for i in 0..20 {
        put(500 + i, 500, 2);
    }
    out
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    /// A zone with **no key table**, `SecurityKickoff` as given, and `caps`
    /// (one bare capability name per line) granted to the `default` group --
    /// which, with `AllowUnknown = yes`, is the group every player here is in.
    fn new(kickoff: bool, caps: &str) -> Self {
        let kick = u8::from(kickoff);
        let mut src = MemSource::new([
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            ("conf/staff.conf", ""),
            (
                "arenas/(default)/arena.conf",
                "[General]\nMap = test.lvl\n[Team]\nDesiredTeams = 2\n",
            ),
        ]);
        src.insert(
            "conf/global.conf",
            format!("[Security]\nSecurityKickoff = {kick}\n"),
        );
        src.insert("conf/groupdef.conf", format!("[default]\n{caps}\n"));
        src.insert("maps/test.lvl", synthetic_map());
        let zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        // The subject of every test in this file is the fallback key path,
        // which is taken only when the zone has no key table. Asserted rather
        // than assumed: a test that quietly ran against a table would be
        // testing the branch that already worked.
        assert!(
            zone.cfg
                .source()
                .read(std::path::Path::new("scrty"))
                .is_none(),
            "these tests are about the tableless path; this zone has a key table"
        );
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

    fn gone(&self, p: PlayerId) -> bool {
        self.zone.players.get(p).is_none()
    }

    /// Logs a VIE player in.
    fn login(&mut self, name: &str) -> PlayerId {
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
        self.send(p, &buf);
        self.tick(10);
        assert!(
            self.take(p)
                .iter()
                .any(|d| d.first() == Some(&s2c::kind::LOGIN_RESPONSE)),
            "the player must be admitted before anything else is measured"
        );
        p
    }

    /// Enters the arena, takes a ship and sends a position: without one the
    /// security module skips the player. Returns the settings packet, which
    /// the settings checksum is computed over.
    fn enter(&mut self, p: PlayerId) -> Vec<u8> {
        let mut buf = Vec::new();
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
        let pkts = self.take(p);
        assert!(
            pkts.iter()
                .any(|d| d.first() == Some(&s2c::kind::ENTERING_ARENA)),
            "the player must actually enter the arena"
        );
        let settings = pkts
            .into_iter()
            .find(|d| d.first() == Some(&s2c::kind::SETTINGS))
            .expect("settings packet");
        self.send(p, &[c2s::kind::SET_SHIP, 0]);
        buf.clear();
        C2SPosition {
            time: u32::try_from(self.t).expect("tick fits"),
            x: 512 * 16,
            y: 512 * 16,
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(5);
        self.take(p);
        settings
    }

    /// Runs on to the next periodic check and returns its key. Stopping
    /// exactly on it matters: running past it runs past the answer deadline
    /// too.
    fn next_challenge(&mut self, p: PlayerId) -> u32 {
        let until = (2500 + INTERVAL - (self.t % INTERVAL)) % INTERVAL;
        self.tick(if until == 0 { INTERVAL } else { until });
        let sec = self
            .take(p)
            .iter()
            .filter(|d| d.first() == Some(&s2c::kind::SECURITY))
            .map(|d| s2c::Security::decode(d).expect("0x18"))
            .next_back()
            .expect("a periodic security check");
        assert_ne!(sec.key, 0, "the periodic check must carry a key");
        sec.key
    }

    /// Answers the current check as a genuine VIE 1.34 client would, with
    /// every field computed for `key`.
    fn answer(&mut self, p: PlayerId, settings: &[u8], key: u32) {
        let mut buf = Vec::new();
        vie_answer(settings, key).encode(&mut buf);
        assert_eq!(buf.len(), SecurityResponse::VIE_LEN);
        self.send(p, &buf);
        self.tick(5);
    }
}

/// What a real VIE 1.34 client answers a check with: every field right for
/// `key`, and `exe_checksum` left at zero because it does not compute one.
fn vie_answer(settings: &[u8], key: u32) -> SecurityResponse {
    let block = SettingsBlock::from_bytes(settings.try_into().expect("1428 bytes"));
    SecurityResponse {
        weapon_count: 1,
        settings_checksum: block.checksum(key),
        exe_checksum: 0,
        map_checksum: ArenaMap::from_bytes(&synthetic_map()).checksum(key),
        s2c_slow_total: 0,
        s2c_fast_total: 0,
        s2c_slow_current: 0,
        s2c_fast_current: 0,
        s2c_reliable_out: 0,
        ping_last: 10,
        ping_average: 10,
        ping_low: 10,
        ping_high: 10,
        slow_frame: 0,
        continuum: None,
    }
}

/// The key a wrong answer was computed for: a different key, still inside the
/// range a key may occupy, whose two checksums both really differ from the
/// right ones.
///
/// Returned rather than applied so each test can assert, before it sends
/// anything, that the answer it is about to send is genuinely wrong. A
/// "wrong" answer that happened to collide would make every assertion below
/// pass for the wrong reason.
fn a_different_key(settings: &[u8], key: u32) -> u32 {
    let other = key ^ 0x5555_5555;
    assert_ne!(other, key);
    assert_ne!(other, 0, "0 is the reserved 'do not answer' value");
    assert!(
        other <= 0x7FFF_FFFF,
        "the wrong key must still be one the implementations score alike"
    );
    let right = vie_answer(settings, key);
    let wrong = vie_answer(settings, other);
    assert_ne!(
        right.settings_checksum, wrong.settings_checksum,
        "the wrong answer must actually differ in the settings checksum"
    );
    assert_ne!(
        right.map_checksum, wrong.map_checksum,
        "the wrong answer must actually differ in the map checksum"
    );
    other
}

// --------------------------------------------------------------- the four

/// **1. The wrong answer is noticed.** On a zone with no key table the server
/// scores a wrong answer `ok = false`, and says which fields disagreed.
///
/// The same player then answers the next check correctly and is scored
/// `ok = true`. That control is the point of the test as much as the mismatch
/// is: without it a server that scored *everything* false would pass.
#[test]
fn a_wrong_answer_on_a_tableless_zone_is_scored_a_mismatch() {
    let (_, log) = with_logs(|| {
        let mut h = H::new(false, "");
        let p = h.login("VieTest134");
        let settings = h.enter(p);

        let key = h.next_challenge(p);
        let wrong = a_different_key(&settings, key);
        h.answer(p, &settings, wrong);

        let key = h.next_challenge(p);
        h.answer(p, &settings, key);
    });

    assert_eq!(
        scores(&log),
        vec![false, true],
        "the wrong answer must be scored a mismatch and the right one not"
    );
    assert!(
        logged(&log, "settings checksum mismatch"),
        "and the server must say the settings checksum disagreed"
    );
    assert!(
        logged(&log, "map checksum mismatch"),
        "and that the map checksum disagreed"
    );
    assert!(
        !logged(&log, "exe checksum mismatch"),
        "the exe arm must not be what caught it: a VIE client reports zero"
    );
}

/// **2. With the kick enabled, the player goes.**
#[test]
fn with_the_kick_enabled_a_wrong_answer_ejects_the_player() {
    let (gone, log) = with_logs(|| {
        let mut h = H::new(true, "");
        let p = h.login("VieTest134");
        let settings = h.enter(p);
        let key = h.next_challenge(p);
        let wrong = a_different_key(&settings, key);
        h.answer(p, &settings, wrong);
        h.gone(p)
    });

    assert_eq!(scores(&log), vec![false], "the answer was scored wrong");
    assert!(
        logged(&log, "kicking off for security violation"),
        "and the kick must be what removed the player, by its own account"
    );
    assert!(
        gone,
        "a wrong answer with SecurityKickoff = 1 is an ejection"
    );
}

/// **2b. And a right answer does not.** The kick in the test above has to be
/// the wrong answer's doing and not something every player in this zone
/// suffers.
#[test]
fn with_the_kick_enabled_a_right_answer_does_not_eject_the_player() {
    let (gone, log) = with_logs(|| {
        let mut h = H::new(true, "");
        let p = h.login("VieTest134");
        let settings = h.enter(p);
        let key = h.next_challenge(p);
        h.answer(p, &settings, key);
        // Past the answer deadline but short of the next check, so silence
        // cannot be mistaken for acceptance.
        h.tick(ANSWER_TIME + 500);
        h.gone(p)
    });

    assert_eq!(scores(&log), vec![true], "the answer was scored right");
    assert!(!logged(&log, "kicking off for security violation"));
    assert!(!gone, "an honest answer survives a zone with the kick on");
}

/// **3. With the kick disabled, the player stays -- and the mismatch is still
/// logged.** The module's doc comment draws exactly this distinction:
/// `SecurityKickoff` gates the ejection, not the detection or the log.
#[test]
fn with_the_kick_disabled_the_player_stays_but_the_mismatch_is_logged() {
    let (gone, log) = with_logs(|| {
        let mut h = H::new(false, "");
        let p = h.login("VieTest134");
        let settings = h.enter(p);
        let key = h.next_challenge(p);
        let wrong = a_different_key(&settings, key);
        h.answer(p, &settings, wrong);
        h.tick(ANSWER_TIME + 500);
        h.gone(p)
    });

    assert!(!gone, "SecurityKickoff = 0 must not eject anybody");
    assert!(
        !logged(&log, "kicking off for security violation"),
        "and must not even claim to"
    );
    assert_eq!(
        scores(&log),
        vec![false],
        "but the answer is still scored a mismatch"
    );
    assert!(
        logged(&log, "settings checksum mismatch"),
        "and the mismatch is still reported"
    );
    assert!(logged(&log, "map checksum mismatch"));
}

/// **4. `bypasssecurity` exempts.** The same zone as test 2 in every respect
/// but one -- the `default` group holds the capability -- and the same wrong
/// answer. `with_the_kick_enabled_a_wrong_answer_ejects_the_player` is the
/// control for this one: it is what shows the kick is otherwise live here.
///
/// The log is asserted to be unchanged: `suppresssecurity` is what silences
/// the log, and an exemption that quietly stopped reporting mismatches would
/// be a different behaviour from the documented one.
#[test]
fn bypasssecurity_exempts_a_player_from_the_kick_but_not_from_the_log() {
    let (gone, log) = with_logs(|| {
        let mut h = H::new(true, BYPASS);
        let p = h.login("VieTest134");
        let settings = h.enter(p);
        let key = h.next_challenge(p);
        let wrong = a_different_key(&settings, key);
        h.answer(p, &settings, wrong);
        h.tick(ANSWER_TIME + 500);
        h.gone(p)
    });

    assert!(!gone, "bypasssecurity must exempt the player from the kick");
    assert!(
        !logged(&log, "kicking off for security violation"),
        "and the kick must not have been attempted"
    );
    assert_eq!(
        scores(&log),
        vec![false],
        "the answer is still scored a mismatch"
    );
    assert!(
        logged(&log, "settings checksum mismatch"),
        "and is still reported: bypasssecurity is not suppresssecurity"
    );
    assert!(logged(&log, "map checksum mismatch"));
}
