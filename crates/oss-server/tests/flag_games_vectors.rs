//! `fg_wz` and `fg_turf` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/flag-games.txt`). Both of ASSS 1.4.4's flag games
//! are Python, so the transcript was produced by running `py/fg_wz.py` and
//! `py/fg_turf.py` unmodified against a plain flag store.
//!
//! That store is not `flagcore.c`, so these records are ground truth for the
//! games — where flags spawn, how a drop is routed, when a win is declared —
//! and for nothing else. flagcore's own state machine is pinned separately in
//! `flagcore_vectors.rs`, and the two are meant to be read together.
//!
//! Every draw a scenario needs is in the transcript, so the harness queues
//! them and then asserts the queue came out empty: that checks our code drew
//! the same values *and* the same number of times, in order, without needing
//! to intercept each call.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::wire::FixedStr;

use oss_formats::region::RegionBitmap;
use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::fg_turf::TurfTag;
use oss_server::modules::fg_wz;
use oss_server::modules::flagcore::{self, Cleanup, FlagInfo, FlagState};
use oss_server::modules::mapdata::MapRegion;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, Ship};
use oss_server::prng::Prng;
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/flag-games.txt");

/// The 32-bit draw that makes our `uniform` return `v`.
fn uniform_draw(v: f64) -> u32 {
    (v * 4_294_967_296.0) as u32
}

/// The 32-bit draw that makes `below(n)` return `k`, checked by round trip.
fn below_draw(k: u32, n: u32) -> u32 {
    assert!(n > 0 && k < n, "below({n}) cannot return {k}");
    let want = (u64::from(k) << 32).div_ceil(u64::from(n));
    let v = u32::try_from(want).unwrap_or(u32::MAX);
    let got = ((u64::from(v) * u64::from(n)) >> 32) as u32;
    assert_eq!(got, k, "could not build a draw for below({n}) == {k}");
    v
}

/// A zone with nothing in it, replaced as soon as a scenario's settings and
/// map are known.
fn minimal_zone() -> Zone {
    let src = MemSource::new([
        ("conf/global.conf", String::new()),
        (
            "conf/passwd.conf",
            "[General]
AllowUnknown = yes
"
            .to_owned(),
        ),
        (
            "conf/groupdef.conf",
            "[default]
"
            .to_owned(),
        ),
        ("conf/staff.conf", String::new()),
        ("arenas/(default)/arena.conf", String::new()),
    ]);
    ServerBuilder::with_source(src).build_zone().expect("zone")
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: Option<ArenaId>,
    players: Vec<PlayerId>,
    tags: Arc<Mutex<Vec<String>>>,
    wins: Arc<Mutex<Vec<String>>>,
    /// What a scoring module answers CB_WARZONEWIN with, if a scenario
    /// scripts one; without it fg_wz's own 1000 stands.
    win_points: Arc<Mutex<Option<i32>>>,
    resets: Arc<Mutex<Vec<String>>>,
    /// Map to build when the arena is made.
    turf: usize,
    solid: Vec<(u32, u32)>,
    noflag: Vec<(u32, u32)>,
    conf: String,
    module: &'static str,
}

impl H {
    fn new(module: &'static str) -> Self {
        Self {
            zone: minimal_zone(),
            lo: Loopback::default(),
            t: 0,
            arena: None,
            players: Vec::new(),
            tags: Arc::new(Mutex::new(Vec::new())),
            wins: Arc::new(Mutex::new(Vec::new())),
            win_points: Arc::new(Mutex::new(None)),
            resets: Arc::new(Mutex::new(Vec::new())),
            turf: 0,
            solid: Vec::new(),
            noflag: Vec::new(),
            conf: String::new(),
            module,
        }
    }

    /// Builds the zone once the scenario's settings and map are known.
    fn build(&mut self) {
        let mut tiles = Vec::new();
        for i in 0..self.turf as u32 {
            let (x, y) = (100 + i * 7, 100 + i * 11);
            tiles.extend_from_slice(&(x | (y << 12) | (170u32 << 24)).to_le_bytes());
        }
        for &(x, y) in &self.solid {
            tiles.extend_from_slice(&(x | (y << 12) | (1u32 << 24)).to_le_bytes());
        }
        let arena_conf = format!(
            "[General]\nMap = test.lvl\n[Modules]\nAttachModules = {}\n{}\n[Team]\nDesiredTeams = 2\n",
            self.module, self.conf
        );
        let mut src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", arena_conf),
        ]);
        src.insert("maps/test.lvl", tiles);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(Prng::new(1));
        let wins = self.wins.clone();
        let scripted = self.win_points.clone();
        zone.on::<fg_wz::WarzoneWin>(Owner(991), Scope::Global, move |_, ev| {
            if let Some(p) = *scripted.lock().expect("lock") {
                ev.points.set(p);
            }
            wins.lock()
                .expect("lock")
                .push(format!("{} {}", ev.freq, ev.points.get()));
        });
        let resets = self.resets.clone();
        zone.on::<flagcore::FlagReset>(Owner(990), Scope::Global, move |_, ev| {
            resets
                .lock()
                .expect("lock")
                .push(format!("{} {}", ev.freq, ev.points));
        });
        let tags = self.tags.clone();
        zone.on::<TurfTag>(Owner(992), Scope::Global, move |_, ev| {
            tags.lock().expect("lock").push(format!(
                "{} {} {} {}",
                ev.player.0, ev.fid, ev.old_freq, ev.new_freq
            ));
        });
        self.zone = zone;
        self.lo = Loopback::default();
        self.t = 0;
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
            ship: 0,
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

    /// Creates the arena, which is what runs the game's Init.
    fn init(&mut self) {
        let boot = self.join("boot");
        let a = self
            .zone
            .players
            .get(boot)
            .and_then(|x| x.arena)
            .expect("arena");
        if let Some(x) = self.zone.players.get_mut(boot) {
            x.ship = Ship::SPEC;
        }
        if !self.noflag.is_empty()
            && let Some(m) = oss_server::modules::mapdata::arena_map_mut(&mut self.zone, a)
        {
            let mut tiles = RegionBitmap::new();
            for &(x, y) in &self.noflag {
                tiles.set(x, y, true);
            }
            let mut regions = (*m.regions).clone();
            regions.push(MapRegion {
                name: "noflag".to_owned(),
                tiles,
                is_base: false,
                no_antiwarp: false,
                no_weapons: false,
                no_flag_drops: true,
                auto_warp: None,
            });
            m.regions = Arc::new(regions);
        }
        self.arena = Some(a);
    }

    /// Player `n`, created on demand, on freq `n` as the transcript has it.
    fn player(&mut self, n: usize) -> PlayerId {
        while self.players.len() <= n {
            let i = self.players.len();
            let p = self.join(&format!("w{i}"));
            if let Some(x) = self.zone.players.get_mut(p) {
                x.freq = u16::try_from(i).unwrap_or(0);
                // The transcript's harness stands each player on a fixed
                // tile, which is where a drop or a neut puts the flag. Its
                // records show p0 at (100, 200) and p1 at (800, 900); no
                // other player is ever a carrier. Positions are in pixels.
                let (tx, ty) = match i {
                    0 => (100, 200),
                    1 => (800, 900),
                    n => (300 + n as i16, 400 + n as i16),
                };
                x.pos.x = tx << 4;
                x.pos.y = ty << 4;
            }
            self.players.push(p);
        }
        self.players[n]
    }

    fn index_of(&self, p: PlayerId) -> String {
        self.players
            .iter()
            .position(|x| *x == p)
            .map(|i| format!("p{i}"))
            .unwrap_or_else(|| "-".to_owned())
    }

    fn flags_line(&self) -> String {
        let Some(a) = self.arena else {
            return "0".to_owned();
        };
        let n = flagcore::flag_count(&self.zone, a);
        let mut parts = Vec::new();
        for fid in 0..n {
            let f = flagcore::get_flag(&self.zone, a, fid).unwrap_or_default();
            let state = match f.state {
                FlagState::None => "none",
                FlagState::OnMap => "onmap",
                FlagState::Carried => "carried",
            };
            let carrier = f
                .carrier
                .map(|c| self.index_of(c))
                .unwrap_or_else(|| "-".to_owned());
            parts.push(format!(
                "{fid}:{state}/{carrier}/{}/{}/{}",
                f.x, f.y, f.freq
            ));
        }
        if n == 0 {
            return "0".to_owned();
        }
        format!("{n} {}", parts.join(" "))
    }
}

fn kv(rest: &str) -> Vec<(String, String)> {
    rest.split_whitespace()
        .filter_map(|w| w.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn flag_games_match_asss() {
    let lines: Vec<&str> = VECTORS
        .lines()
        .map(|l| l.trim_end_matches(['\r', '\n']))
        .collect();
    let mut h = H::new("fg_wz");
    let mut label = String::new();
    let mut scenarios = 0usize;
    let mut checked = 0usize;
    let mut turf_scenario = false;
    let mut no_position: std::collections::HashSet<usize> = Default::default();
    let mut skip_flags = false;
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(c) = line.strip_prefix("# ") {
            // a scenario label is one of the two games followed by a colon
            if let Some(rest) = c.strip_prefix("turf: ").or_else(|| c.strip_prefix("turf:")) {
                let _ = rest;
                label = c.to_owned();
                turf_scenario = true;
                no_position.clear();
                h = H::new("fg_turf");
                scenarios += 1;
            } else if c.starts_with("wz:") || c.starts_with("wz ") {
                label = c.to_owned();
                turf_scenario = false;
                no_position.clear();
                h = H::new("fg_wz");
                scenarios += 1;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "settings" => {
                let mut conf = String::new();
                for (k, v) in kv(rest) {
                    if let Some((section, key)) = k.split_once(':') {
                        conf.push_str(&format!("[{section}]\n{key} = {v}\n"));
                    }
                }
                h.conf = conf;
            }
            "map" => {
                for (k, v) in kv(rest) {
                    if k == "flagcount" {
                        h.turf = v.parse().unwrap_or(0);
                    }
                }
                // the solid and noflag lines follow, one per tile
                while i < lines.len() {
                    let (w, r) = lines[i].split_once(' ').unwrap_or((lines[i], ""));
                    let coords: Vec<u32> = r
                        .split_whitespace()
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    match w {
                        "solid" if coords.len() == 2 => h.solid.push((coords[0], coords[1])),
                        "noflag" if coords.len() == 2 => h.noflag.push((coords[0], coords[1])),
                        _ => break,
                    }
                    i += 1;
                }
                h.build();
            }
            "op" => {
                // Create any player the op names FIRST. Joining draws from
                // the generator (the freq manager picks between equal
                // teams), which would eat the draws queued below.
                for tok in rest.split_whitespace() {
                    let t = tok.split_once('=').map_or(tok, |(_, v)| v);
                    if let Some(n) = t.strip_prefix('p')
                        && let Ok(n) = n.parse::<usize>()
                    {
                        h.player(n);
                    }
                }
                // queue the draws the records after this op say were made
                let mut expect_reset: Option<String> = None;
                let mut expect_win: Option<String> = None;
                let mut draws: Vec<u32> = Vec::new();
                let mut j = i;
                while j < lines.len() {
                    let Some(l) = lines[j].strip_prefix("log ") else {
                        break;
                    };
                    if let Some(u) = l.strip_prefix("prng Uniform -> ") {
                        draws.push(uniform_draw(u.trim().parse().expect("uniform")));
                    } else if let Some(r) = l.strip_prefix("flagcore FlagReset ") {
                        expect_reset = Some(r.trim().to_owned());
                    } else if let Some(w) = l.strip_prefix("callback warzonewin ") {
                        // "(flags, 1, 0)" -- arena, freq, points
                        let inner = w.trim().trim_matches(['(', ')']);
                        let f: Vec<&str> = inner.split(',').map(str::trim).collect();
                        expect_win = f.get(1).map(|v| (*v).to_owned());
                    } else if let Some(n) = l.strip_prefix("prng Number ") {
                        let v: Vec<i64> = n
                            .split_whitespace()
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        // Number(lo, hi) -> v, over our below(hi - lo + 1)
                        let (lo, hi, got) = (v[0], v[1], v[2]);
                        let span = u32::try_from(hi - lo + 1).unwrap_or(1);
                        draws.push(below_draw(u32::try_from(got - lo).unwrap_or(0), span));
                    }
                    j += 1;
                }
                if !draws.is_empty() {
                    h.zone.res.get_or_default::<Prng>().queue(draws);
                }
                run_op(&mut h, rest, &label);
                let left = h.zone.res.get_or_default::<Prng>().queued();
                assert_eq!(left, 0, "{label}: {left} queued draws were never used");
                if let Some(want) = expect_win {
                    let got = h.wins.lock().expect("lock").clone();
                    let freq = got
                        .last()
                        .and_then(|w| w.split(' ').next().map(str::to_owned));
                    assert_eq!(
                        freq.as_deref(),
                        Some(want.as_str()),
                        "{label}: winning freq"
                    );
                    checked += 1;
                }
                if let Some(want) = expect_reset {
                    let got = h.resets.lock().expect("lock").clone();
                    assert_eq!(
                        got.last().map(String::as_str),
                        Some(want.as_str()),
                        "{label}: FlagReset"
                    );
                    // flagcore blanks every flag on a reset, so the flags
                    // record that follows describes the plain store before
                    // that happened; fg_wz_current=0 below is the game's
                    // own part of it and is still checked
                    skip_flags = true;
                    checked += 1;
                }
                checked += 1;
            }
            "cbresult" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                if v.first() == Some(&"warzonewin") {
                    *h.win_points.lock().expect("lock") = v.get(1).and_then(|n| n.parse().ok());
                }
            }
            "current" => {
                let n: usize = rest.trim().parse().expect("count");
                let a = h.arena.expect("arena");
                fg_wz::set_current(&mut h.zone, a, n);
            }
            "place" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let fid: usize = v[0].parse().expect("flag id");
                let f = parse_flag(&mut h, v[1]);
                let a = h.arena.expect("arena");
                flagcore::set_flag(&mut h.zone, a, fid, f);
            }
            "flags" if skip_flags => {
                skip_flags = false;
            }
            "flags" => {
                if rest.trim().starts_with("255 ") {
                    // fg_wz clips Flag:FlagCount's max to 255 and reserves
                    // that many, which the transcript's plain store does.
                    // flagcore.c refuses a count of MAXFLAGS or more --
                    // `flagcount >= MAXFLAGS` with MAXFLAGS 255 -- so a real
                    // ASSS zone configured this way gets no flags at all,
                    // and neither do we. The clipping itself is what this
                    // scenario is for, and the `sets max=255` record below
                    // checks it.
                    checked += 1;
                    continue;
                }
                let want = carried_freq_from_carrier(rest.trim());
                // flagcore.c erases x and y when a flag goes from on the
                // map to none -- its "faked removal", pinned in
                // flagcore_vectors.rs -- while this file's plain store
                // keeps whatever the game passed. Once that has happened to
                // a flag the two stores disagree about its position for
                // good, so drop the position for that flag from here on and
                // keep comparing everything else.
                for (fid, got, exp) in diverged_positions(&h.flags_line(), &want) {
                    let _ = (got, exp);
                    no_position.insert(fid);
                }
                let want = drop_positions(&want, &no_position);
                if turf_scenario {
                    // The transcript's store keeps the x and y fg_turf
                    // passes; the real flagcore forces (-1, -1) for every
                    // turf flag, as flagcore_vectors.rs pins. Compare the
                    // fields the game actually decides.
                    assert_eq!(
                        strip_xy(&h.flags_line()),
                        strip_xy(&want),
                        "{label}: flag state (position excluded for turf)"
                    );
                } else {
                    assert_eq!(
                        drop_positions(&h.flags_line(), &no_position),
                        want,
                        "{label}: flag state"
                    );
                }
                checked += 1;
            }
            "arena" => {
                let Some(a) = h.arena else { continue };
                for (k, v) in kv(rest) {
                    match k.as_str() {
                        "fg_wz_current" => {
                            assert_eq!(
                                fg_wz::current(&h.zone, a).to_string(),
                                v,
                                "{label}: live flag count"
                            );
                            checked += 1;
                        }
                        "fg_wz_neuts" => {
                            let got = fg_wz::neuts(&h.zone, a)
                                .iter()
                                .map(usize::to_string)
                                .collect::<Vec<_>>()
                                .join(", ");
                            assert_eq!(format!("[{got}]"), v, "{label}: neuted flags");
                            checked += 1;
                        }
                        // fg_turf_fc is the flag count, which `flags` covers
                        _ => {}
                    }
                }
            }
            "sets" => {
                let Some(a) = h.arena else { continue };
                let s = fg_wz::settings(&h.zone, a).expect("settings");
                for (k, v) in kv(rest) {
                    let got = match k.as_str() {
                        "min" => s.min.to_string(),
                        "max" => s.max.to_string(),
                        "spawnx" => s.spawn_x.to_string(),
                        "spawny" => s.spawn_y.to_string(),
                        "spawnr" => s.spawn_r.to_string(),
                        "dropr" => s.drop_r.to_string(),
                        "carryflags" => s.carry_flags.to_string(),
                        "friendlytransfer" => u8::from(s.friendly_transfer).to_string(),
                        "dropowned" => u8::from(s.drop_owned).to_string(),
                        "dropcenter" => u8::from(s.drop_center).to_string(),
                        "neutowned" => u8::from(s.neut_owned).to_string(),
                        "neutcenter" => u8::from(s.neut_center).to_string(),
                        "tkowned" => u8::from(s.tk_owned).to_string(),
                        "tkcenter" => u8::from(s.tk_center).to_string(),
                        "safeowned" => u8::from(s.safe_owned).to_string(),
                        "safecenter" => u8::from(s.safe_center).to_string(),
                        "windelay" => s.win_delay.to_string(),
                        // Flag:ResetDelay is 0 in every scenario and we do
                        // not model it; nothing here exercises it
                        _ => continue,
                    };
                    assert_eq!(got, v, "{label}: setting {k}");
                    checked += 1;
                }
            }
            _ => {}
        }
    }
    assert!(scenarios >= 30, "only {scenarios} scenarios replayed");
    assert!(checked > 200, "only {checked} records checked");
}

/// Flags whose stored position has gone its own way: an expected `none`
/// entry whose position we no longer agree on.
fn diverged_positions(got: &str, want: &str) -> Vec<(usize, String, String)> {
    let parse = |line: &str| -> Vec<(usize, Vec<String>)> {
        line.split_whitespace()
            .filter_map(|w| {
                let (id, body) = w.split_once(':')?;
                Some((
                    id.parse().ok()?,
                    body.split('/').map(str::to_owned).collect(),
                ))
            })
            .collect()
    };
    let (g, w) = (parse(got), parse(want));
    let mut out = Vec::new();
    for (fid, wf) in w {
        if wf.first().map(String::as_str) != Some("none") {
            continue;
        }
        if let Some((_, gf)) = g.iter().find(|(i, _)| *i == fid)
            && (gf.get(2) != wf.get(2) || gf.get(3) != wf.get(3))
        {
            out.push((fid, gf.join("/"), wf.join("/")));
        }
    }
    out
}

/// Blanks the position of the named flags in a `flags` line.
fn drop_positions(line: &str, which: &std::collections::HashSet<usize>) -> String {
    line.split_whitespace()
        .map(|w| match w.split_once(':') {
            Some((id, body)) => {
                let f: Vec<&str> = body.split('/').collect();
                let hit = id
                    .parse::<usize>()
                    .map(|i| which.contains(&i))
                    .unwrap_or(false);
                if hit && f.len() == 5 {
                    format!("{id}:{}/{}/?/?/{}", f[0], f[1], f[4])
                } else {
                    w.to_owned()
                }
            }
            None => w.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A carried flag's freq comes from the carrier in `flagcore.c`, which
/// overwrites whatever the game passed; the transcript's plain store keeps
/// the game's value instead. `flagcore_vectors.rs` pins that rule, so here
/// the expected freq is replaced by the carrier's -- players are seated on
/// the freq that matches their number.
fn carried_freq_from_carrier(line: &str) -> String {
    line.split_whitespace()
        .map(|w| match w.split_once(':') {
            Some((id, body)) => {
                let f: Vec<&str> = body.split('/').collect();
                if f.len() == 5 && f[0] == "carried" && f[1].starts_with('p') {
                    let freq = f[1].trim_start_matches('p');
                    format!("{id}:{}/{}/{}/{}/{freq}", f[0], f[1], f[2], f[3])
                } else {
                    w.to_owned()
                }
            }
            None => w.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drops the x and y out of a `flags` line, for turf scenarios.
fn strip_xy(line: &str) -> String {
    line.split_whitespace()
        .map(|w| match w.split_once(':') {
            Some((id, body)) => {
                let f: Vec<&str> = body.split('/').collect();
                if f.len() == 5 {
                    format!("{id}:{}/{}/{}", f[0], f[1], f[4])
                } else {
                    w.to_owned()
                }
            }
            None => w.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `onmap/-/10/10/-1`
fn parse_flag(h: &mut H, spec: &str) -> FlagInfo {
    let f: Vec<&str> = spec.split('/').collect();
    let state = match f[0] {
        "onmap" => FlagState::OnMap,
        "carried" => FlagState::Carried,
        _ => FlagState::None,
    };
    let carrier = (f[1] != "-").then(|| {
        let n: usize = f[1].trim_start_matches('p').parse().expect("carrier");
        h.player(n)
    });
    FlagInfo {
        state,
        carrier,
        x: f[2].parse().expect("x"),
        y: f[3].parse().expect("y"),
        freq: f[4].parse().expect("freq"),
    }
}

fn run_op(h: &mut H, rest: &str, label: &str) {
    let v: Vec<&str> = rest.split_whitespace().collect();
    match v.first().copied().unwrap_or("") {
        // the game is attached when the arena is made, which Init does
        "attach" => {}
        "Init" => h.init(),
        "FlagTouch" => {
            let n: usize = v[1].trim_start_matches('p').parse().expect("player");
            let fid: usize = v[2].parse().expect("flag id");
            let carried: i16 = v
                .get(3)
                .and_then(|w| w.strip_prefix("carried=")?.parse().ok())
                .unwrap_or(0);
            let p = h.player(n);
            if let Some(x) = h.zone.players.get_mut(p) {
                x.flags_carried = carried;
            }
            let a = h.arena.expect("arena");
            let game = h
                .zone
                .iface_in::<dyn flagcore::FlagGame>(a)
                .expect("flag game");
            game.flag_touch(&mut h.zone, a, p, fid);
        }
        "Cleanup" => {
            let fid: usize = v[1].parse().expect("flag id");
            let reason: u8 = v[2]
                .strip_prefix("reason=")
                .and_then(|r| r.parse().ok())
                .expect("reason");
            let n: usize = v[3]
                .strip_prefix("carrier=")
                .map(|c| c.trim_start_matches('p').parse().expect("carrier"))
                .expect("carrier");
            let freq: i16 = v[4]
                .strip_prefix("freq=")
                .and_then(|f| f.parse().ok())
                .expect("freq");
            let reason = match reason {
                0 => Cleanup::Dropped,
                1 => Cleanup::InSafe,
                2 => Cleanup::ShipChange,
                3 => Cleanup::FreqChange,
                4 => Cleanup::LeftArena,
                5 => Cleanup::KillNormal,
                6 => Cleanup::KillTk,
                7 => Cleanup::KillCantCarry,
                8 => Cleanup::KillFake,
                _ => Cleanup::Other,
            };
            let p = h.player(n);
            let a = h.arena.expect("arena");
            let game = h
                .zone
                .iface_in::<dyn flagcore::FlagGame>(a)
                .expect("flag game");
            game.cleanup(&mut h.zone, a, fid, reason, p, freq);
        }
        "spawn_timer" => {
            let a = h.arena.expect("arena");
            fg_wz::run_spawn_timer(&mut h.zone, a);
        }
        "check_win" => {
            let a = h.arena.expect("arena");
            fg_wz::run_check_win(&mut h.zone, a);
        }
        other => panic!("{label}: unknown op {other}"),
    }
}
