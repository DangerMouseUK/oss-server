//! The zone nobody configured must not accept any name with any password.
//!
//! Background, because the behaviour under test is deliberately a divergence
//! from the behavioural reference rather than a bug fix. ASSS ships
//! `conf/passwd.conf` with `AllowUnknown = yes` and an empty `[Users]`, and
//! Subspace Server .NET does the same with the same default; an unlisted
//! name is then let in with any password. That is a public SubSpace zone and
//! it is still available here, but it is no longer what a zone that was
//! never configured does, because `oss-server` with no arguments (and
//! `docker run` with no arguments) is now a one-command zone a stranger can
//! reach. See decision `D29`.
//!
//! Every test here drives a real login packet through the real module set,
//! so what is asserted is the login response code a client would receive.

use oss_proto::Packet;
use oss_proto::c2s::{self, Login};
use oss_proto::s2c::{self, LoginResponse};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::auth_file::hash_password;
use oss_server::modules::billing_ssc::{BillerConnector, BillerLink};
use oss_server::modules::core::{self as core_mod, auth_code};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const GLOBAL: &str = "[Chat]\nMessageReliable = 1\n";
const ARENA: &str = "[Misc]\nBounceFactor = 22\n[Modules]\nAttachModules = \n";
const GROUPDEF: &str = "[default]\ncmd_version = 1\n[mod]\ncmd_version = 1\ncmd_arena\n";

/// A zone built from `passwd.conf` text, with the builder tweaked by `f`.
struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new(passwd: &str) -> Self {
        Self::built(passwd, |b| b)
    }

    fn built(passwd: &str, f: impl FnOnce(ServerBuilder) -> ServerBuilder) -> Self {
        Self::with_staff(passwd, "[(global)]\n", f)
    }

    fn with_staff(
        passwd: &str,
        staff: &str,
        f: impl FnOnce(ServerBuilder) -> ServerBuilder,
    ) -> Self {
        let src = MemSource::new([
            ("conf/global.conf", GLOBAL.to_owned()),
            ("conf/passwd.conf", passwd.to_owned()),
            ("conf/groupdef.conf", GROUPDEF.to_owned()),
            ("conf/staff.conf", staff.to_owned()),
            ("arenas/(default)/arena.conf", ARENA.to_owned()),
        ]);
        let zone = f(ServerBuilder::with_source(src))
            .build_zone()
            .expect("zone");
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

    fn login(&mut self, name: &str, pw: &str) -> (PlayerId, u8) {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        login_packet(name, pw).encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.lo.collect(&mut self.zone);
        self.tick(10);
        let code = self
            .lo
            .take(p)
            .iter()
            .find(|(d, _)| d[0] == s2c::kind::LOGIN_RESPONSE)
            .map(|(d, _)| LoginResponse::decode(d).expect("login response").code)
            .expect("no login response");
        (p, code)
    }

    /// The zone password the server installed, as the running zone sees it.
    fn zone_password(&mut self) -> Option<String> {
        let f = self.zone.cfg.open(None, "passwd.conf").ok()?;
        f.get("General", "ZonePassword").map(str::to_owned)
    }
}

fn login_packet(name: &str, pw: &str) -> Login {
    Login {
        kind: c2s::kind::LOGIN,
        flags: 0,
        name: FixedStr::new(name.as_bytes()),
        password: FixedStr::new(pw.as_bytes()),
        machine_id: 1234,
        connect_type: 4,
        timezone_bias: 0,
        unknown1: 0,
        client_version: 134,
        field444: 444,
        bound_ip: 0,
        permission_id: 99,
        server_ip: 0,
        bound_port: 0,
        redirect_id: 0,
        using_proxy: 0,
        unused: 0,
        cont_id: None,
    }
}

// ---------------------------------------------------------------------------
// The default: a zone nobody configured
// ---------------------------------------------------------------------------

/// The headline. `AllowUnknown = yes` with an empty `[Users]` is exactly the
/// file the bundled zone ships, and it must no longer admit a stranger.
#[test]
fn an_unconfigured_zone_refuses_a_stranger() {
    let mut h = H::new("[General]\nAllowUnknown = yes\n[Users]\n");
    let (_, code) = h.login("stranger", "whatever");
    assert_eq!(
        code,
        auth_code::BAD_PASSWORD,
        "a zone with no login configuration must not accept an arbitrary password"
    );
}

/// ...and the zone is still usable: the generated password works.
#[test]
fn the_generated_zone_password_lets_anyone_in() {
    let mut h = H::new("[General]\nAllowUnknown = yes\n[Users]\n");
    let pw = h.zone_password().expect("a zone password was generated");
    assert_eq!(pw.len(), 16, "generated password length");
    assert!(
        pw.bytes()
            .all(|b| b"abcdefghijkmnpqrstuvwxyz23456789".contains(&b)),
        "generated password uses the unambiguous alphabet: {pw}"
    );
    let (_, code) = h.login("anyone", &pw);
    assert!(auth_code::is_ok(code), "code {code}");
}

/// Two zones must not be handed the same password. This is a weak test of a
/// strong property -- it catches a constant or a fixed seed, which is the
/// failure that would actually happen -- and is not a statistical claim.
#[test]
fn generated_passwords_differ_between_zones() {
    let conf = "[General]\nAllowUnknown = yes\n[Users]\n";
    let a = H::new(conf).zone_password().expect("a");
    let b = H::new(conf).zone_password().expect("b");
    assert_ne!(a, b);
}

/// The zone password says which zone you may enter, not who you are. If it
/// made players authenticated, anyone holding it could log in under a name
/// listed in `staff.conf` and collect its powers -- which is the hole ASSS's
/// own `passwd.conf` warns about in prose and this asserts in code.
#[test]
fn the_zone_password_does_not_confer_staff() {
    let mut h = H::with_staff(
        "[General]\nAllowUnknown = yes\n[Users]\n",
        "[(global)]\nboss = mod\n",
        |b| b,
    );
    let pw = h.zone_password().expect("generated");
    let (p, code) = h.login("boss", &pw);
    assert!(auth_code::is_ok(code), "code {code}");
    let player = h.zone.players.get(p).expect("player");
    assert!(
        !player.authenticated,
        "a zone-password login must never be marked authenticated"
    );
}

// ---------------------------------------------------------------------------
// Configurations the operator chose: all left alone
// ---------------------------------------------------------------------------

/// A hand-written zone password is used as written and nothing is generated.
#[test]
fn a_configured_zone_password_is_kept() {
    let mut h = H::new("[General]\nAllowUnknown = yes\nZonePassword = hunter2\n[Users]\n");
    assert_eq!(h.zone_password().as_deref(), Some("hunter2"));
    assert!(auth_code::is_ok(h.login("anyone", "hunter2").1));
    assert_eq!(h.login("anyone else", "wrong").1, auth_code::BAD_PASSWORD);
}

/// `AllowAnyLogin` is the old behaviour, restored on purpose. This is the
/// developer mode: a nickname and any password at all.
#[test]
fn allow_any_login_in_the_config_file_opens_the_zone() {
    let mut h = H::new("[General]\nAllowUnknown = yes\nAllowAnyLogin = yes\n[Users]\n");
    assert_eq!(h.zone_password(), None, "nothing generated");
    assert!(auth_code::is_ok(h.login("whoever", "anything").1));
}

/// ...and the same thing from the command line, for one run.
#[test]
fn allow_any_login_on_the_command_line_opens_the_zone() {
    let mut h = H::built("[General]\nAllowUnknown = yes\n[Users]\n", |b| {
        b.allow_any_login(true)
    });
    assert_eq!(h.zone_password(), None, "nothing generated");
    assert!(auth_code::is_ok(h.login("whoever", "anything").1));
}

/// `--zone-password` overrides a generated one, so a container that keeps
/// nothing across restarts can keep the password its players have.
#[test]
fn the_command_line_zone_password_wins() {
    let mut h = H::built("[General]\nAllowUnknown = yes\n[Users]\n", |b| {
        b.zone_password("chosen-by-the-operator")
    });
    assert_eq!(h.zone_password().as_deref(), Some("chosen-by-the-operator"));
    assert!(auth_code::is_ok(
        h.login("anyone", "chosen-by-the-operator").1
    ));
    assert_eq!(
        h.login("anyone", "something else").1,
        auth_code::BAD_PASSWORD
    );
}

/// A private zone is untouched: `AllowUnknown = no` still refuses unlisted
/// names outright rather than acquiring a zone password it does not want.
#[test]
fn allow_unknown_no_is_untouched() {
    let mut h = H::new("[General]\nAllowUnknown = no\n[Users]\n");
    assert_eq!(h.zone_password(), None, "nothing generated");
    assert_eq!(h.login("stranger", "whatever").1, auth_code::NO_PERMISSION2);
}

// ---------------------------------------------------------------------------
// ASSS semantics for listed names: entirely unchanged
// ---------------------------------------------------------------------------

/// A listed name with a hash is checked against its own password, is
/// authenticated when right, and never sees the zone password.
#[test]
fn a_listed_player_keeps_their_own_password() {
    let passwd = format!(
        "[General]\nAllowUnknown = yes\n[Users]\nbob = {}\n",
        hash_password("bob", "secret")
    );
    let mut h = H::new(&passwd);
    let zp = h.zone_password().expect("generated for everyone else");
    let (p, code) = h.login("bob", "secret");
    assert!(auth_code::is_ok(code), "code {code}");
    assert!(
        h.zone.players.get(p).expect("player").authenticated,
        "a correct listed password still authenticates"
    );
    // ...and the zone password is not a way into a listed name.
    assert_eq!(h.login("bob", &zp).1, auth_code::BAD_PASSWORD);
}

/// `lock` and `any` are ASSS's own special values and keep working.
#[test]
fn lock_and_any_are_unchanged() {
    let mut h = H::new("[General]\nAllowUnknown = yes\n[Users]\nbanned = lock\nguest = any\n");
    assert_eq!(h.login("banned", "whatever").1, auth_code::NO_PERMISSION);
    assert!(auth_code::is_ok(h.login("guest", "whatever").1));
}

/// A populated `[Users]` does NOT close the door for everyone else -- which
/// is why the generated password is installed for a zone that has one. The
/// inverse of this test is the mistake it exists to prevent.
#[test]
fn listing_users_does_not_make_a_zone_closed() {
    let passwd = format!(
        "[General]\nAllowUnknown = yes\n[Users]\nbob = {}\n",
        hash_password("bob", "secret")
    );
    let mut h = H::new(&passwd);
    assert!(
        h.zone_password().is_some(),
        "an unlisted name could still log in with any password, so a zone \
         password is still needed"
    );
    assert_eq!(h.login("not bob", "anything").1, auth_code::BAD_PASSWORD);
}

// ---------------------------------------------------------------------------
// The artefact itself: the zone `oss-server` and `docker run` produce
// ---------------------------------------------------------------------------

/// The bundled zone is the one compiled into the binary, and is what both
/// `oss-server` with no arguments and `docker run` with no arguments give
/// you. It is the reason this change exists, so it is asserted against
/// directly rather than against a fixture that resembles it.
#[test]
fn the_bundled_zone_refuses_a_stranger_and_admits_its_own_password() {
    let mut zone = ServerBuilder::default_zone().build_zone().expect("zone");
    let pw = zone
        .cfg
        .open(None, "passwd.conf")
        .expect("passwd.conf")
        .get("General", "ZonePassword")
        .expect("the bundled zone generated a zone password")
        .to_owned();
    assert_eq!(pw.len(), 16);

    let mut lo = Loopback::default();
    let mut t = 0u64;
    let attempt = |zone: &mut Zone, lo: &mut Loopback, t: &mut u64, name: &str, pass: &str| {
        let p = core_mod::new_connection(zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        login_packet(name, pass).encode(&mut buf);
        zone.handle_packet(p, &buf);
        lo.collect(zone);
        for _ in 0..10 {
            *t += 1;
            zone.set_now(Ticks(*t));
            oss_server::timers::run_due(zone);
            lo.collect(zone);
        }
        lo.take(p)
            .iter()
            .find(|(d, _)| d[0] == s2c::kind::LOGIN_RESPONSE)
            .map(|(d, _)| LoginResponse::decode(d).expect("login response").code)
            .expect("no login response")
    };

    assert_eq!(
        attempt(&mut zone, &mut lo, &mut t, "stranger", "anything"),
        auth_code::BAD_PASSWORD,
        "docker run with no arguments must not give a stranger a zone"
    );
    assert!(auth_code::is_ok(attempt(
        &mut zone, &mut lo, &mut t, "stranger", &pw
    )));
}

/// And the bundled zone's own `passwd.conf`, read as a file, must not be
/// shipping a state this change forbids. This catches an edit to
/// `zone/conf/passwd.conf` that re-opens the zone without anyone noticing,
/// which a behavioural test alone would not: a shipped `AllowAnyLogin = yes`
/// would make every test above pass and the zone open.
#[test]
fn the_shipped_passwd_conf_does_not_open_the_zone() {
    let text = String::from_utf8_lossy(
        oss_server::default_zone::FILES
            .iter()
            .find(|(p, _)| *p == "conf/passwd.conf")
            .map(|(_, b)| *b)
            .expect("conf/passwd.conf is bundled"),
    )
    .into_owned();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with(';') || l.is_empty() {
            continue;
        }
        let key = l.split('=').next().unwrap_or_default().trim();
        assert!(
            !key.eq_ignore_ascii_case("AllowAnyLogin"),
            "the shipped passwd.conf must not set AllowAnyLogin: {line}"
        );
        assert!(
            !key.eq_ignore_ascii_case("ZonePassword"),
            "the shipped passwd.conf must not ship a password every copy of \
             this server would share: {line}"
        );
    }
}

// ---------------------------------------------------------------------------
// With a biller: what the local keys do, which is nothing
// ---------------------------------------------------------------------------
//
// The lead asked whether a biller's answer replaces the local check,
// overrides it, or adds to it. It replaces it, structurally and not by
// policy: `billing_ssc` registers its own `Auth` with the zone registry, the
// registry keeps the last provider for an interface, and `modules.conf`
// lists `billing_ssc` after `auth_file`. From then on `auth_file` is loaded,
// holds `passwd.conf`, answers `?passwd` -- and is never asked who may log
// in. The two tests below are that claim in executable form.

/// A biller that can never be reached, so the link is permanently down.
struct NeverConnects;

impl BillerConnector for NeverConnects {
    fn connect(&self, _addr: std::net::SocketAddr) -> Option<Box<dyn BillerLink>> {
        None
    }
}

const BILLING_GLOBAL: &str = "[Chat]\nMessageReliable = 1\n[Listen]\nPort = 5000\n\
[Billing]\nIP = 127.0.0.1\nPort = 1850\nServerID = 5000\nGroupID = 1\nScoreID = 5000\n\
ServerName = OSS Test Zone\nPassword = billpw\nRetryInterval = 5\n";

/// A zone with both `auth_file` and `billing_ssc` loaded, in `modules.conf`
/// order, and a biller that never answers.
fn billed_zone(passwd: &str) -> Zone {
    let src = MemSource::new([
        ("conf/global.conf", BILLING_GLOBAL.to_owned()),
        ("conf/passwd.conf", passwd.to_owned()),
        ("conf/groupdef.conf", GROUPDEF.to_owned()),
        ("conf/staff.conf", "[(global)]\n".to_owned()),
        ("arenas/(default)/arena.conf", ARENA.to_owned()),
    ]);
    let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert!(modules.iter().any(|m| m == "auth_file"));
    modules.push("billing_ssc".into());
    let mut zone = ServerBuilder::with_source(src)
        .modules(modules)
        .build_zone()
        .expect("zone");
    zone.reg.provide::<dyn BillerConnector>(
        oss_server::registry::Owner(9999),
        oss_server::registry::Scope::Global,
        std::sync::Arc::new(NeverConnects),
    );
    zone
}

/// A biller replaces `auth_file` as the zone's authenticator, so `auth_file`
/// must stand down rather than generate a zone password nothing would read.
#[test]
fn a_biller_stops_auth_file_generating_a_zone_password() {
    let mut zone = billed_zone("[General]\nAllowUnknown = yes\n[Users]\n");
    let pw = zone
        .cfg
        .open(None, "passwd.conf")
        .expect("passwd.conf")
        .get("General", "ZonePassword")
        .map(str::to_owned);
    assert_eq!(
        pw, None,
        "a zone whose logins the biller decides must not acquire a zone \
         password: nothing would ever read it"
    );
}

/// And the door that *is* open in the biller case is the biller module's
/// own, not this one's: with the link down, `billing_ssc` admits any name
/// with any password under a `^` prefix, unauthenticated. That is ASSS
/// 1.4.4's `billing_ssc.c` behaviour and this change does not touch it --
/// recorded here so the next person reads it from a test rather than
/// rediscovering it. See D29, "what this does not decide".
#[test]
fn a_down_biller_admits_anyone_under_a_caret_and_the_local_keys_do_not_apply() {
    let mut zone = billed_zone("[General]\nAllowUnknown = no\n[Users]\n");
    let mut lo = Loopback::default();
    let p = core_mod::new_connection(&mut zone, ClientKind::Vie, None).expect("slot");
    let mut buf = Vec::new();
    login_packet("stranger", "any password at all").encode(&mut buf);
    zone.handle_packet(p, &buf);
    lo.collect(&mut zone);
    for t in 1..=10u64 {
        zone.set_now(Ticks(t));
        oss_server::timers::run_due(&mut zone);
        lo.collect(&mut zone);
    }
    let code = lo
        .take(p)
        .iter()
        .find(|(d, _)| d[0] == s2c::kind::LOGIN_RESPONSE)
        .map(|(d, _)| LoginResponse::decode(d).expect("login response").code)
        .expect("no login response");
    assert!(
        auth_code::is_ok(code),
        "the biller's own fallback let them in, despite AllowUnknown = no: code {code}"
    );
    assert_eq!(
        zone.players.get(p).map(|x| x.name.clone()).as_deref(),
        Some("^stranger"),
        "the caret marks a player the biller did not vouch for"
    );
    assert!(
        !zone.players.get(p).expect("player").authenticated,
        "and they are not authenticated, so they get no staff group"
    );
}
