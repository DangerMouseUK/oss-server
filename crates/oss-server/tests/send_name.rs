//! The name other players are shown is the auth module's `send_name`, not
//! the name of record (ASSS `AuthData::sendname` versus `AuthData::name`).
//!
//! ASSS keeps the two apart so a module can decorate what a client sees
//! without touching what scores, bans, groups and staff commands key on.
//! Both halves are checked here, with an auth module that deliberately makes
//! them differ, because a port that collapsed them would still pass every
//! test in which they happen to be equal -- which is every other test.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::PlayerData;
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::module::{Module, ModuleError};
use oss_server::modules::core as core_mod;
use oss_server::modules::core::{Auth, AuthData, LoginRequest};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::{Zone, ZoneStarted};

/// The one name this stand-in decorates, and what it shows for it.
const NAME: &str = "staffer";
const SHOWN: &str = "+staffer";

/// Stands in for an auth module that decorates the displayed name, which is
/// what ASSS's `auth_prefix` was meant to be. Nothing in the tree does this
/// today, so without it the two names could never be observed to differ.
///
/// Everyone else it answers the way ASSS's own auth modules do, by leaving
/// `send_name` empty -- which must still produce a name on the wire.
struct DecoratingAuth;

impl Auth for DecoratingAuth {
    fn authenticate(&self, zone: &mut Zone, player: PlayerId, login: &LoginRequest) {
        let mut ad = AuthData::ok(&login.name);
        if login.name == NAME {
            ad.send_name = SHOWN.to_owned();
        } else {
            ad.send_name.clear();
        }
        core_mod::auth_done(zone, player, ad);
    }
}

struct DecoratingAuthModule;

impl Module for DecoratingAuthModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Auth>(me, Scope::Global, Arc::new(DecoratingAuth));
        Ok(())
    }
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new() -> Self {
        let src = MemSource::new([
            ("conf/global.conf", ""),
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            ("conf/groupdef.conf", "[default]\ncmd_go = 1\n"),
            ("conf/staff.conf", ""),
            ("arenas/(default)/arena.conf", "[Team]\nDesiredTeams = 2\n"),
        ]);
        let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        modules.push("decorating_auth".into());
        let mut zone = ServerBuilder::with_source(src)
            .module("decorating_auth", &[], || DecoratingAuthModule)
            .modules(modules)
            .build_zone()
            .expect("zone");
        zone.fire(&ZoneStarted);
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
        }
    }

    fn tick(&mut self) {
        self.t += 1;
        self.zone.set_now(Ticks(self.t));
        oss_server::timers::run_due(&mut self.zone);
        self.lo.collect(&mut self.zone);
    }

    fn join(&mut self, typed: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(typed.as_bytes()),
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
        for _ in 0..10 {
            self.tick();
        }
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
        for _ in 0..10 {
            self.tick();
        }
        p
    }

    /// Every name in a player-data packet this player was sent.
    fn names_seen_by(&mut self, p: PlayerId) -> Vec<String> {
        self.lo
            .take(p)
            .into_iter()
            // several records may arrive concatenated in one packet
            .filter_map(|(d, _)| PlayerData::decode_all(&d).ok())
            .flatten()
            .map(|e| {
                String::from_utf8_lossy(
                    e.name
                        .as_bytes()
                        .split(|b| *b == 0)
                        .next()
                        .unwrap_or_default(),
                )
                .into_owned()
            })
            .collect()
    }
}

#[test]
fn clients_are_shown_send_name_and_the_server_keeps_name() {
    let mut h = H::new();
    let watcher = h.join("watcher");
    h.lo.take(watcher);
    // note the player cannot type the prefix: `core.rs` refuses a name that
    // does not start with a letter or number, which is why ASSS's own
    // auth_prefix is unreachable. Decoration has to come from the auth
    // module, as it does here.
    let staffer = h.join(NAME);

    // the name of record is the auth module's `name`
    assert_eq!(
        h.zone.players.get(staffer).map(|p| p.name.clone()),
        Some(NAME.to_owned()),
        "the player is known to the server by the name auth returned"
    );
    // and the arena was told the decorated one
    let seen = h.names_seen_by(watcher);
    assert!(
        seen.contains(&SHOWN.to_owned()),
        "another player should have been shown {SHOWN}, saw {seen:?}"
    );
    assert!(
        !seen.contains(&NAME.to_owned()),
        "the undecorated name reached a client: {seen:?}"
    );
}

#[test]
fn an_auth_module_that_sets_no_send_name_is_shown_by_name() {
    // ASSS's own auth modules leave `sendname` equal to `name`, and some
    // leave it empty. Either way a client must still be told a name: a port
    // that simply copied the field through would show nobody at all.
    let mut h = H::new();
    let watcher = h.join("watcher");
    h.lo.take(watcher);
    h.join("plain");
    let seen = h.names_seen_by(watcher);
    assert!(
        seen.contains(&"plain".to_owned()),
        "an empty send_name should fall back to the name, saw {seen:?}"
    );
}
