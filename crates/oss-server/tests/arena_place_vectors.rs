//! `ap_multipub` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/arena-place.txt`, from `core/ap_multipub.c`
//! compiled unmodified).
//!
//! The rules worth pinning: an arena that already exists and has room
//! always beats creating a new one, even when the new one comes first in
//! the list; `General:DesiredPlaying` is read from the arena being
//! considered rather than from global; the count compared is the number in
//! ships, so spectators never keep anyone out; and both the name and the
//! `<name><pass>` built from it are cut to 19 characters, which can make
//! two configured names collide on the same arena.

use std::sync::Arc;

use oss_server::config::{ConfigFile, MemSource};
use oss_server::modules::arenaman::ArenaPlace;
use oss_server::modules::core as core_mod;
use oss_server::player::{ClientKind, PlayerId, Ship};
use oss_server::runtime::ServerBuilder;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/arena-place.txt");

struct H {
    zone: Zone,
}

impl H {
    fn new(public_arenas: &str) -> Self {
        let src = MemSource::new([
            (
                "conf/global.conf",
                format!("[General]\nPublicArenas = {public_arenas}\n"),
            ),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", String::new()),
        ]);
        let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        modules.push("ap_multipub".to_owned());
        let zone = ServerBuilder::with_source(src)
            .modules(modules)
            .build_zone()
            .expect("zone");
        Self { zone }
    }

    /// Makes an arena exist with the population the record describes.
    /// `desired` of -1 leaves `General:DesiredPlaying` unset.
    fn arena(&mut self, name: &str, total: usize, playing: usize, desired: i32) {
        let id = self.zone.arenas.create(name);
        let text = if desired < 0 {
            String::new()
        } else {
            format!("[General]\nDesiredPlaying = {desired}\n")
        };
        if let Some(a) = self.zone.arenas.get_mut(id) {
            a.cfg = Arc::new(ConfigFile::from_text(&text));
        }
        for i in 0..total {
            let p = self.connect(&format!("{name}_{i}"));
            if let Some(x) = self.zone.players.get_mut(p) {
                x.arena = Some(id);
                x.status = oss_server::player::PlayerStatus::Playing;
                // the record's 'playing' count is the one in ships; the
                // rest are spectators, who must not keep anyone out
                x.ship = if i < playing { Ship(0) } else { Ship::SPEC };
            }
        }
    }

    /// A connection with no arena, enough to be counted once placed.
    fn connect(&mut self, name: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        if let Some(x) = self.zone.players.get_mut(p) {
            x.name = name.to_owned();
        }
        p
    }

    fn place(&mut self, connect_as: Option<&str>) -> Option<String> {
        let p = self.connect("asker");
        if let Some(c) = connect_as
            && let Some(x) = self.zone.players.get_mut(p)
        {
            x.connect_as = Some(c.to_owned());
        }
        let place = self.zone.iface::<dyn ArenaPlace>().expect("ap_multipub");
        place.place(&mut self.zone, p)
    }
}

/// The first `[...]` of a record.
fn first_bracketed(s: &str) -> Option<(String, &str)> {
    let open = s.find('[')?;
    let close = s[open..].find(']')? + open;
    Some((s[open + 1..close].to_owned(), &s[close + 1..]))
}

#[test]
fn arena_placement_matches_asss() {
    let mut h: Option<H> = None;
    let mut label = String::new();
    let mut checked = 0usize;
    let mut scenarios = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            if let Some((text, rest)) = c.rsplit_once(": General:PublicArenas = ") {
                let (setting, _) = first_bracketed(rest).unwrap_or_default();
                label = text.to_owned();
                h = Some(H::new(&setting));
                scenarios += 1;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "pubnames" => {
                let hh = h.as_ref().expect("a scenario first");
                let (n, mut tail) = rest.split_once(' ').unwrap_or((rest, ""));
                let n: usize = n.trim().parse().expect("name count");
                let mut want: Vec<String> = Vec::new();
                while let Some((name, next)) = first_bracketed(tail) {
                    want.push(name);
                    tail = next;
                }
                assert_eq!(want.len(), n, "the record disagrees with itself");
                let got: Vec<String> = hh
                    .zone
                    .cfg
                    .global()
                    .get("General", "PublicArenas")
                    .unwrap_or("")
                    .split([' ', ',', ':', ';'])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.chars().take(19).collect())
                    .collect();
                assert_eq!(got, want, "{label}: parsed public arena names");
                checked += 1;
            }
            "arena" => {
                let hh = h.as_mut().expect("a scenario first");
                let v: Vec<&str> = rest.split_whitespace().collect();
                hh.arena(
                    v[0],
                    v[1].parse().expect("total"),
                    v[2].parse().expect("playing"),
                    v[3].parse().expect("desired"),
                );
            }
            "place" => {
                let hh = h.as_mut().expect("a scenario first");
                let (who, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                let (ret, tail) = tail.split_once(' ').unwrap_or((tail, ""));
                let ret: i32 = ret.parse().expect("return value");
                let (want, _) = first_bracketed(tail).expect("arena name");
                let connect_as = (who != "-").then_some(who);
                let got = hh.place(connect_as);
                if ret == 0 {
                    assert!(
                        got.is_none(),
                        "{label}: placed in {got:?}, ASSS placed nobody"
                    );
                } else {
                    // ASSS returns 1 when an existing arena won and the first
                    // character of the backup name otherwise. We return the
                    // name either way; what the number distinguishes -- found
                    // versus fell back -- is not a difference a caller here
                    // can act on, and 0 for "nowhere" maps to None.
                    assert_eq!(got.as_deref(), Some(want.as_str()), "{label}: placement");
                }
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(scenarios >= 14, "only {scenarios} scenarios replayed");
    assert!(checked >= 28, "only {checked} records checked");
}
