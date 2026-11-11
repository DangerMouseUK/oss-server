//! Changing arena advertises the new arena's LVZ list, not the old one.
//!
//! The initial join is covered elsewhere; this is the change from one
//! arena's `General:LevelFiles` to a different one, which is the case the
//! bundled zone cannot show because it ships no `.lvz` at all.

use oss_formats::lvz::{CLV2, Lvz, ObjectSection, Section};
use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::objects::ObjectData;
use oss_proto::s2c::{self, MapFilename};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::{Zone, ZoneStarted};

/// A real, if tiny, LVZ holding one map object, so the two files differ in
/// content and therefore in checksum.
fn lvz(id: u16) -> Vec<u8> {
    let section = ObjectSection {
        version: CLV2,
        objects: vec![ObjectData::map(id, 100, 200)],
        images: Vec::new(),
    };
    Lvz {
        sections: vec![Section::objects(&section)],
    }
    .to_bytes()
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new() -> Self {
        let mut src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\ncmd_go = 1\n".to_owned()),
            ("conf/staff.conf", String::new()),
            (
                "arenas/alpha/arena.conf",
                "[General]\nLevelFiles = alpha.lvz\n[Team]\nDesiredTeams = 2\n".to_owned(),
            ),
            (
                "arenas/beta/arena.conf",
                "[General]\nLevelFiles = beta.lvz\n[Team]\nDesiredTeams = 2\n".to_owned(),
            ),
            (
                "arenas/(default)/arena.conf",
                "[Team]\nDesiredTeams = 2\n".to_owned(),
            ),
        ]);
        src.insert("maps/alpha.lvz", lvz(1));
        src.insert("maps/beta.lvz", lvz(2));
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        zone.fire(&ZoneStarted);
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
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
            cont_id: None,
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        let _ = self.lo.take(p);
        p
    }

    /// Asks for a named arena. `optional_graphics` is what makes the server
    /// send the whole file list rather than just the map: a VIE client that
    /// did not ask gets only the map name, as in ASSS.
    fn go(&mut self, p: PlayerId, arena: &str) -> Vec<Vec<u8>> {
        let mut buf = Vec::new();
        GoArena {
            ship: 8,
            allow_audio: 1,
            obscenity_filter: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: -3,
            arena: FixedStr::new(arena.as_bytes()),
            optional_graphics: Some(1),
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(20);
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }

    /// The arena the player is actually in, so a test cannot pass by the
    /// move having silently not happened.
    fn arena_of(&self, p: PlayerId) -> String {
        self.zone
            .players
            .get(p)
            .and_then(|x| x.arena)
            .and_then(|a| self.zone.arenas.get(a))
            .map(|a| a.name.clone())
            .unwrap_or_default()
    }
}

/// The names in the last map-filename packet of `pkts`.
fn advertised(pkts: &[Vec<u8>]) -> Vec<String> {
    let mf = pkts
        .iter()
        .rfind(|d| d[0] == s2c::kind::MAP_FILENAME)
        .expect("a map-filename packet");
    match MapFilename::decode(mf).expect("decode") {
        MapFilename::Files(files) => files
            .iter()
            .map(|f| {
                String::from_utf8_lossy(
                    f.name
                        .as_bytes()
                        .split(|b| *b == 0)
                        .next()
                        .unwrap_or_default(),
                )
                .into_owned()
            })
            .collect(),
        MapFilename::Single { name, .. } => vec![
            String::from_utf8_lossy(
                name.as_bytes()
                    .split(|b| *b == 0)
                    .next()
                    .unwrap_or_default(),
            )
            .into_owned(),
        ],
    }
}

#[test]
fn changing_arena_advertises_the_new_arenas_lvz_list() {
    let mut h = H::new();
    let p = h.login("alice");

    let first = advertised(&h.go(p, "alpha"));
    assert_eq!(h.arena_of(p), "alpha");
    assert!(
        first.iter().any(|n| n == "alpha.lvz"),
        "alpha's LVZ was not advertised: {first:?}"
    );

    let second = advertised(&h.go(p, "beta"));
    assert_eq!(h.arena_of(p), "beta", "the arena change did not happen");
    assert!(
        second.iter().any(|n| n == "beta.lvz"),
        "beta's LVZ was not advertised on the change: {second:?}"
    );
    assert!(
        !second.iter().any(|n| n == "alpha.lvz"),
        "the old arena's LVZ was still advertised after the change: {second:?}"
    );
}

#[test]
fn an_arena_with_no_lvz_advertises_none_after_one_that_had_them() {
    // the other direction, which is the one that leaves a stale object on
    // screen if the client is told nothing: moving to an arena whose list
    // is empty must still produce a packet, naming the map alone
    let mut h = H::new();
    let p = h.login("alice");
    let first = advertised(&h.go(p, "alpha"));
    assert!(first.iter().any(|n| n == "alpha.lvz"), "{first:?}");

    let second = advertised(&h.go(p, "plain"));
    assert_eq!(h.arena_of(p), "plain", "the arena change did not happen");
    assert!(
        !second.iter().any(|n| n.ends_with(".lvz")),
        "an arena with no LevelFiles advertised one: {second:?}"
    );
    assert!(
        !second.is_empty(),
        "the client must still be told the map name"
    );
}

/// The same check against the **bundled zone** rather than a synthetic
/// fixture: `scf` serves one LVZ and `(default)` serves none, which is the
/// pair a live client is moved between.
mod bundled {
    use super::advertised;
    use oss_proto::Packet;
    use oss_proto::c2s::{self, GoArena, Login};
    use oss_proto::wire::FixedStr;
    use oss_server::modules::core as core_mod;
    use oss_server::net::Loopback;
    use oss_server::player::{ClientKind, PlayerId};
    use oss_server::runtime::ServerBuilder;
    use oss_server::time::Ticks;
    use oss_server::zone::{Zone, ZoneStarted};

    struct B {
        zone: Zone,
        lo: Loopback,
        t: u64,
    }

    impl B {
        fn new() -> Self {
            let mut zone = ServerBuilder::default_zone().build_zone().expect("zone");
            zone.res.insert(oss_server::prng::Prng::new(1));
            zone.fire(&ZoneStarted);
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

        fn login(&mut self) -> PlayerId {
            let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
            let mut buf = Vec::new();
            Login {
                kind: c2s::kind::LOGIN,
                flags: 0,
                name: FixedStr::new(b"alice"),
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
            let _ = self.lo.take(p);
            p
        }

        fn go(&mut self, p: PlayerId, arena: &str) -> Vec<Vec<u8>> {
            let mut buf = Vec::new();
            GoArena {
                ship: 8,
                allow_audio: 1,
                obscenity_filter: 0,
                x_res: 1024,
                y_res: 768,
                arena_type: -3,
                arena: FixedStr::new(arena.as_bytes()),
                optional_graphics: Some(1),
            }
            .encode(&mut buf);
            self.zone.handle_packet(p, &buf);
            self.tick(20);
            self.lo.take(p).into_iter().map(|(d, _)| d).collect()
        }

        fn arena_of(&self, p: PlayerId) -> String {
            self.zone
                .players
                .get(p)
                .and_then(|x| x.arena)
                .and_then(|a| self.zone.arenas.get(a))
                .map(|a| a.name.clone())
                .unwrap_or_default()
        }
    }

    #[test]
    fn scf_serves_its_lvz_and_a_public_arena_does_not() {
        let mut b = B::new();
        let p = b.login();

        let in_scf = advertised(&b.go(p, "scf"));
        assert_eq!(b.arena_of(p), "scf");
        assert!(
            in_scf.iter().any(|n| n == "scf-demo.lvz"),
            "scf should serve its LVZ: {in_scf:?}"
        );
        assert!(
            in_scf.iter().any(|n| n == "scf.lvl"),
            "and its map: {in_scf:?}"
        );

        let in_public = advertised(&b.go(p, "0"));
        assert_eq!(b.arena_of(p), "0", "the arena change did not happen");
        assert!(
            !in_public.iter().any(|n| n.ends_with(".lvz")),
            "a public arena serves no LVZ, so the client must be told none: {in_public:?}"
        );
    }

    #[test]
    fn the_committed_lvz_holds_what_its_readme_says() {
        // The binary in the tree is the one thing here a reviewer cannot
        // read, so this states its contents in source: two images we drew
        // and two objects. `cargo run --example make_zone_lvz` rewrites it
        // deterministically, so a reviewer can also regenerate and diff.
        use oss_formats::lvz::{Lvz, ObjectSection};
        let bytes = oss_server::default_zone::FILES
            .iter()
            .find(|(name, _)| *name == "arenas/scf/scf-demo.lvz")
            .map(|(_, data)| *data)
            .expect("the zone ships scf-demo.lvz");
        let file = Lvz::parse(bytes).expect("a valid LVZ");
        let names: Vec<String> = file
            .sections
            .iter()
            .map(|s| String::from_utf8_lossy(&s.name).into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["oss-green.bmp", "oss-blue.bmp", ""],
            "two image files and one object section"
        );
        for s in file.sections.iter().filter(|s| !s.name.is_empty()) {
            let raw = s.data().expect("inflate");
            assert_eq!(&raw[..2], b"BM", "the images are BMPs we generated");
        }
        let objs = file
            .sections
            .iter()
            .find(|s| s.is_object_section())
            .map(|s| ObjectSection::parse(&s.data().expect("inflate")).expect("objects"))
            .expect("an object section");
        assert_eq!(
            objs.images
                .iter()
                .map(|i| String::from_utf8_lossy(&i.name).into_owned())
                .collect::<Vec<_>>(),
            vec!["oss-green.bmp", "oss-blue.bmp"]
        );
        let ids: Vec<u16> = objs
            .objects
            .iter()
            .map(oss_proto::objects::ObjectData::id)
            .collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(
            objs.objects
                .iter()
                .all(oss_proto::objects::ObjectData::is_map_object),
            "both are map objects, so no screen-layout question arises"
        );
    }
}
