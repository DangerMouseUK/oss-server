//! More of ASSS `playercmd.c`: arena/zone messages, information and game
//! control commands. Capabilities follow cmdman (`cmd_<name>` in public,
//! `privcmd_<name>` aimed at a player or team).
//!
//! Commands aimed at a target act on that player, everyone on that team, or
//! (typed in public) everyone in the arena, as in ASSS.

use oss_proto::Packet;
use oss_proto::s2c::{ArenaEntry, ArenaList, PrizeReceive, ShipReset, WarpTo};

use crate::arena::ArenaStatus;
use crate::commands::{Invocation, Target};
use crate::config::to_latin1;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus, Ship};
use crate::registry::Owner;
use crate::zone::Zone;

use super::chat::{Chat, message};

/// Players a command applies to.
pub fn targets(zone: &Zone, inv: &Invocation<'_>) -> Vec<PlayerId> {
    match inv.target {
        Target::Player(p) => vec![p],
        Target::Freq(a, f) => zone
            .players
            .in_arena(a)
            .filter(|p| p.freq == f)
            .map(|p| p.id)
            .collect(),
        Target::Arena(a) => zone.players.in_arena(a).map(|p| p.id).collect(),
        Target::None => Vec::new(),
    }
}

fn chat(zone: &Zone) -> Option<std::sync::Arc<dyn Chat>> {
    zone.iface::<dyn Chat>()
}

fn sender_name(zone: &Zone, inv: &Invocation<'_>) -> String {
    zone.players
        .get(inv.sender)
        .map(|p| p.name.clone())
        .unwrap_or_default()
}

fn send_to<P: Packet>(zone: &mut Zone, to: &[PlayerId], pkt: &P) {
    let mut buf = Vec::with_capacity(8);
    pkt.encode(&mut buf);
    zone.send_many(to.iter().copied(), &buf, SendFlags::RELIABLE);
}

// ---- messages -------------------------------------------------------------

fn c_a(zone: &mut Zone, inv: &Invocation<'_>) {
    let to = targets(zone, inv);
    let text = format!("{}  -{}", inv.params, sender_name(zone, inv));
    if let Some(c) = chat(zone) {
        c.send_set_message(zone, &to, &text);
    }
}

fn c_aa(zone: &mut Zone, inv: &Invocation<'_>) {
    let to = targets(zone, inv);
    if let Some(c) = chat(zone) {
        c.send_set_message(zone, &to, inv.params);
    }
}

fn zone_message(zone: &mut Zone, text: &str) {
    let to: Vec<PlayerId> = zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing)
        .map(|p| p.id)
        .collect();
    if let Some(c) = chat(zone) {
        c.send_set_message(zone, &to, text);
    }
}

fn c_z(zone: &mut Zone, inv: &Invocation<'_>) {
    let text = format!("{}  -{}", inv.params, sender_name(zone, inv));
    zone_message(zone, &text);
}

fn c_az(zone: &mut Zone, inv: &Invocation<'_>) {
    zone_message(zone, inv.params);
}

fn c_warn(zone: &mut Zone, inv: &Invocation<'_>) {
    let Target::Player(t) = inv.target else {
        message(zone, inv.sender, "You must use this on a player.");
        return;
    };
    let text = format!("WARNING: {}  -{}", inv.params, sender_name(zone, inv));
    if let Some(c) = chat(zone) {
        c.send_any_message(
            zone,
            &[t],
            oss_proto::common::chat_type::WARNING,
            0,
            -1,
            &text,
        );
    }
    message(zone, inv.sender, "Player warned.");
}

// ---- information ------------------------------------------------------------

fn c_arena(zone: &mut Zone, inv: &Invocation<'_>) {
    let mine = zone.players.get(inv.sender).and_then(|p| p.arena);
    let all = inv.params.contains("-a") || inv.params.contains("all");
    let see_private = zone
        .iface::<dyn super::capman::Capabilities>()
        .is_some_and(|c| c.has_capability(zone, inv.sender, "seeprivarena"));
    let mut arenas = Vec::new();
    for a in zone.arenas.iter() {
        if a.status != ArenaStatus::Running {
            continue;
        }
        let private = a.name.starts_with('#');
        if private && !see_private && Some(a.id) != mine {
            continue;
        }
        let pop = zone
            .players
            .iter()
            .filter(|p| p.arena == Some(a.id))
            .count() as i16;
        if pop == 0 && !all {
            continue;
        }
        let population = if Some(a.id) == mine { -pop } else { pop };
        arenas.push(ArenaEntry {
            name: to_latin1(&a.name),
            population,
        });
    }
    send_to(zone, &[inv.sender], &ArenaList { arenas });
}

fn c_uptime(zone: &mut Zone, inv: &Invocation<'_>) {
    let secs = zone.now().0 / 100;
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    message(
        zone,
        inv.sender,
        &format!("uptime: {d} days {h} hours {m} minutes {s} seconds"),
    );
}

fn c_info(zone: &mut Zone, inv: &Invocation<'_>) {
    let Target::Player(t) = inv.target else {
        message(zone, inv.sender, "info: must use on a player");
        return;
    };
    let Some(p) = zone.players.get(t) else { return };
    let arena = p
        .arena
        .and_then(|a| zone.arenas.get(a))
        .map_or("(none)".to_owned(), |a| a.name.clone());
    let lines = [
        format!(
            "{}: pid={} status={:?} name='{}' squad='{}'",
            p.name, p.id.0, p.status, p.name, p.squad
        ),
        format!(
            "{}: arena={} ship={} freq={} client={} authenticated={}",
            p.name, arena, p.ship.0, p.freq, p.client_name, p.authenticated
        ),
        format!(
            "{}: connected {} s ago, ip={}",
            p.name,
            zone.now().since(p.connected_at) / 100,
            p.addr.map_or("(none)".to_owned(), |a| a.ip().to_string())
        ),
    ];
    for l in lines {
        message(zone, inv.sender, &l);
    }
}

fn c_where(zone: &mut Zone, inv: &Invocation<'_>) {
    let t = match inv.target {
        Target::Player(t) => t,
        _ => inv.sender,
    };
    let Some(p) = zone.players.get(t) else { return };
    let (x, y) = (i32::from(p.pos.x) >> 4, i32::from(p.pos.y) >> 4);
    let col = (b'A' + (x * 20 / 1024).clamp(0, 19) as u8) as char;
    let row = (y * 20 / 1024).clamp(0, 19) + 1;
    let who = if t == inv.sender {
        "You are".to_owned()
    } else {
        format!("{} is", p.name)
    };
    let text = if p.ship.is_spec() && t != inv.sender {
        format!("{} not in a ship.", p.name)
    } else {
        format!("{who} at {col}{row} ({x},{y})")
    };
    message(zone, inv.sender, &text);
}

fn c_find(zone: &mut Zone, inv: &Invocation<'_>) {
    let name = inv.params.trim();
    let found = zone
        .players
        .find_by_name(name)
        .map(|p| (p.name.clone(), p.arena));
    let text = match found {
        Some((n, Some(a))) => {
            let an = zone
                .arenas
                .get(a)
                .map(|x| x.name.clone())
                .unwrap_or_default();
            if an.starts_with('#') {
                format!("{n} is in a private arena.")
            } else {
                format!("{n} is in arena {an}.")
            }
        }
        Some((n, None)) => format!("{n} is logged in but not in an arena."),
        None if zone.commands.default_handler().is_some() => {
            // not here: ask the biller (ASSS sends "\find" to the default handler)
            let line = format!("\\find {name}");
            super::cmdman::command(zone, inv.sender, &line, inv.target);
            return;
        }
        None => format!("{name} isn't online."),
    };
    message(zone, inv.sender, &text);
}

fn c_lsmod(zone: &mut Zone, inv: &Invocation<'_>) {
    let mods: Vec<String> = zone.modules.loaded().map(str::to_owned).collect();
    message(
        zone,
        inv.sender,
        &format!("loaded modules: {}", mods.join(", ")),
    );
}

fn c_listmod(zone: &mut Zone, inv: &Invocation<'_>) {
    let groups = zone.iface::<dyn super::capman::Groups>();
    let mut lines = Vec::new();
    for p in zone.players.iter() {
        let g = groups
            .as_ref()
            .map(|g| g.group(zone, p.id))
            .unwrap_or_default();
        if g != super::capman::DEFAULT_GROUP && g != "none" {
            let arena = p
                .arena
                .and_then(|a| zone.arenas.get(a))
                .map_or(String::new(), |a| a.name.clone());
            lines.push(format!("{:>20} {:>10} {}", p.name, arena, g));
        }
    }
    for l in lines {
        message(zone, inv.sender, &l);
    }
}

fn c_mapinfo(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let map = zone
        .arenas
        .get(a)
        .and_then(|x| x.ext.get::<super::mapnewsdl::ArenaDownloads>())
        .and_then(|d| d.0.first().map(|m| (m.name.clone(), m.checksum, m.raw_len)));
    let text = match map {
        Some((n, crc, len)) => format!("LVL file '{n}': {len} bytes, checksum {crc:08x}"),
        None => "No map loaded.".to_owned(),
    };
    message(zone, inv.sender, &text);
}

// ---- game control -------------------------------------------------------------

fn c_setship(zone: &mut Zone, inv: &Invocation<'_>) {
    let Ok(n) = inv.params.trim().parse::<u8>() else {
        message(
            zone,
            inv.sender,
            "setship: give a ship number 1-8 (8 = spec)... or 0 for spec",
        );
        return;
    };
    let ship = if n == 0 || n >= 9 {
        Ship::SPEC
    } else {
        Ship(n - 1)
    };
    for p in targets(zone, inv) {
        let Some(pl) = zone.players.get(p) else {
            continue;
        };
        let freq = if ship.is_spec() {
            pl.arena
                .and_then(|a| zone.arenas.get(a))
                .map_or(8025, super::arenaman::spec_freq)
        } else {
            pl.freq
        };
        super::game::set_ship_and_freq(zone, p, ship, freq);
    }
}

fn c_setfreq(zone: &mut Zone, inv: &Invocation<'_>) {
    let Ok(freq) = inv.params.trim().parse::<u16>() else {
        message(zone, inv.sender, "setfreq: give a frequency");
        return;
    };
    for p in targets(zone, inv) {
        super::game::set_freq(zone, p, freq);
    }
}

fn c_specall(zone: &mut Zone, inv: &Invocation<'_>) {
    for p in targets(zone, inv) {
        let sf = zone
            .players
            .get(p)
            .and_then(|x| x.arena)
            .and_then(|a| zone.arenas.get(a))
            .map_or(8025, super::arenaman::spec_freq);
        super::game::set_ship_and_freq(zone, p, Ship::SPEC, sf);
    }
}

fn c_warpto(zone: &mut Zone, inv: &Invocation<'_>) {
    let mut it = inv
        .params
        .split([' ', ','])
        .filter(|s| !s.is_empty())
        .map(str::parse::<i16>);
    let (Some(Ok(x)), Some(Ok(y))) = (it.next(), it.next()) else {
        message(zone, inv.sender, "warpto: give x and y tile coordinates");
        return;
    };
    let to = targets(zone, inv);
    send_to(zone, &to, &WarpTo { x, y });
}

fn c_shipreset(zone: &mut Zone, inv: &Invocation<'_>) {
    let to = targets(zone, inv);
    send_to(zone, &to, &ShipReset {});
}

fn c_prize(zone: &mut Zone, inv: &Invocation<'_>) {
    // "?prize [count] <prize>" or "?prize #<n>"; negative prizes take away
    let words: Vec<&str> = inv.params.split_whitespace().collect();
    let (count, prize) = match words.as_slice() {
        [p] => (1, p.trim_start_matches('#').parse::<i16>().ok()),
        [c, p] => (
            c.parse::<i16>().unwrap_or(1),
            p.trim_start_matches('#').parse::<i16>().ok(),
        ),
        _ => (1, None),
    };
    let Some(prize) = prize else {
        message(
            zone,
            inv.sender,
            "prize: give a prize number (1-28, negative to take away)",
        );
        return;
    };
    let to = targets(zone, inv);
    send_to(zone, &to, &PrizeReceive { count, prize });
}

fn c_lock(zone: &mut Zone, inv: &Invocation<'_>) {
    let spec = inv.params.contains("-s");
    for p in targets(zone, inv) {
        super::game::set_lock(zone, p, true, spec);
    }
    message(zone, inv.sender, "Locked.");
}

fn c_unlock(zone: &mut Zone, inv: &Invocation<'_>) {
    for p in targets(zone, inv) {
        super::game::set_lock(zone, p, false, false);
    }
    message(zone, inv.sender, "Unlocked.");
}

fn c_send(zone: &mut Zone, inv: &Invocation<'_>) {
    let Target::Player(t) = inv.target else {
        message(zone, inv.sender, "send: must use on a player");
        return;
    };
    let Some(p) = zone.players.get(t) else { return };
    let req = super::arenaman::GoRequest {
        name: inv.params.trim().to_owned(),
        ship: p.ship,
        resolution: p.resolution,
        want_all_lvz: p.want_all_lvz,
        accept_audio: p.accept_audio,
        obscenity_filter: p.obscenity_filter,
    };
    super::arenaman::go(zone, t, req);
}

fn c_shutdown(zone: &mut Zone, inv: &Invocation<'_>) {
    tracing::info!(target: "staffcmd", by = %sender_name(zone, inv), "shutdown requested");
    zone_message(zone, "The server is shutting down.");
    zone.request_shutdown();
}

fn c_reloadconf(zone: &mut Zone, inv: &Invocation<'_>) {
    let n = zone.cfg.reload_changed().len();
    message(
        zone,
        inv.sender,
        &format!("Reloaded {n} changed config file(s)."),
    );
}

fn c_flagreset(zone: &mut Zone, inv: &Invocation<'_>) {
    if let Some(a) = inv.arena {
        super::flagcore::flag_reset(zone, a, -1, 0);
    }
}

fn c_ballcount(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    match inv.params.trim() {
        "" => {
            let n = super::balls::balls(zone, a).len();
            message(zone, inv.sender, &format!("Ball count: {n}"));
        }
        s => match s.parse::<usize>() {
            Ok(n) => super::balls::set_ball_count(zone, a, n),
            Err(_) => message(zone, inv.sender, "ballcount: give a number"),
        },
    }
}

fn c_jackpot(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let jp = zone
        .iface::<dyn super::jackpot::Jackpot>()
        .map_or(0, |j| j.get(zone, a));
    message(zone, inv.sender, &format!("The jackpot is {jp} points."));
}

fn c_setjackpot(zone: &mut Zone, inv: &Invocation<'_>) {
    let (Some(a), Ok(n)) = (inv.arena, inv.params.trim().parse::<i32>()) else {
        message(zone, inv.sender, "setjackpot: give a number");
        return;
    };
    if let Some(j) = zone.iface::<dyn super::jackpot::Jackpot>() {
        j.set(zone, a, n);
        message(zone, inv.sender, &format!("Jackpot set to {n} points."));
    }
}

fn c_scorereset(zone: &mut Zone, inv: &Invocation<'_>) {
    // resetting someone else needs the privcmd capability; your own needs
    // Misc:SelfScoreReset
    let players = match inv.target {
        Target::Player(t) => vec![t],
        _ => {
            let allowed = inv
                .arena
                .and_then(|a| zone.arenas.get(a))
                .is_some_and(|a| a.cfg.get_int("Misc", "SelfScoreReset", 0) != 0);
            if !allowed {
                message(
                    zone,
                    inv.sender,
                    "This arena doesn't allow you to reset your own scores.",
                );
                return;
            }
            vec![inv.sender]
        }
    };
    let Some(st) = zone.iface::<dyn super::stats::Stats>() else {
        return;
    };
    use super::stats::stat;
    for p in players {
        for s in [
            stat::KILL_POINTS,
            stat::FLAG_POINTS,
            stat::KILLS,
            stat::DEATHS,
        ] {
            st.set(zone, p, s, 0);
        }
        message(zone, p, "Your scores have been reset.");
    }
    st.send_updates(zone, None);
}

fn c_points(zone: &mut Zone, inv: &Invocation<'_>) {
    let Ok(n) = inv.params.trim().parse::<i32>() else {
        message(zone, inv.sender, "points: give a number");
        return;
    };
    let Some(st) = zone.iface::<dyn super::stats::Stats>() else {
        return;
    };
    for p in targets(zone, inv) {
        st.increment(zone, p, super::stats::stat::FLAG_POINTS, n);
    }
    st.send_updates(zone, None);
}

fn c_getgroup(zone: &mut Zone, inv: &Invocation<'_>) {
    let t = match inv.target {
        Target::Player(t) => t,
        _ => inv.sender,
    };
    let g = zone
        .iface::<dyn super::capman::Groups>()
        .map(|g| g.group(zone, t))
        .unwrap_or_default();
    let name = zone
        .players
        .get(t)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    message(zone, inv.sender, &format!("{name} is in group {g}."));
}

fn c_setgroup(zone: &mut Zone, inv: &Invocation<'_>) {
    let Target::Player(t) = inv.target else {
        message(zone, inv.sender, "setgroup: must use on a player");
        return;
    };
    let words: Vec<&str> = inv.params.split_whitespace().collect();
    let Some(group) = words.iter().find(|w| !w.starts_with('-')).copied() else {
        message(zone, inv.sender, "setgroup: give a group name");
        return;
    };
    let caps = zone.iface::<dyn super::capman::Capabilities>();
    // you need higher_than_<group> to hand out a group, and to touch the target
    let ok = caps.is_some_and(|c| {
        c.has_capability(zone, inv.sender, &format!("higher_than_{group}"))
            && c.higher_than(zone, inv.sender, t)
    });
    if !ok {
        message(
            zone,
            inv.sender,
            "You don't have permission to give people that group.",
        );
        return;
    }
    let global = words.contains(&"-a") || words.contains(&"-global");
    let temp = words.contains(&"-t") || words.contains(&"-temp");
    let Some(g) = zone.iface::<dyn super::capman::Groups>() else {
        return;
    };
    let by = sender_name(zone, inv);
    if temp {
        g.set_temp_group(zone, t, group);
    } else {
        g.set_perm_group(zone, t, group, global, &format!("set by {by}"));
    }
    let name = zone
        .players
        .get(t)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    message(
        zone,
        inv.sender,
        &format!("{name} assigned to group {group}."),
    );
    message(
        zone,
        t,
        &format!("You have been assigned to group {group} by {by}."),
    );
}

fn c_rmgroup(zone: &mut Zone, inv: &Invocation<'_>) {
    let Target::Player(t) = inv.target else {
        message(zone, inv.sender, "rmgroup: must use on a player");
        return;
    };
    let ok = zone
        .iface::<dyn super::capman::Capabilities>()
        .is_some_and(|c| c.higher_than(zone, inv.sender, t));
    if !ok {
        message(
            zone,
            inv.sender,
            "You don't have permission to remove that player's group.",
        );
        return;
    }
    let by = sender_name(zone, inv);
    if let Some(g) = zone.iface::<dyn super::capman::Groups>() {
        g.remove_group(zone, t, &format!("removed by {by}"));
    }
    message(zone, inv.sender, "Group removed.");
}

fn c_grplogin(zone: &mut Zone, inv: &Invocation<'_>) {
    let mut it = inv.params.split_whitespace();
    let (Some(group), Some(pw)) = (it.next(), it.next()) else {
        message(zone, inv.sender, "grplogin: give a group name and password");
        return;
    };
    let Some(g) = zone.iface::<dyn super::capman::Groups>() else {
        return;
    };
    if g.check_group_password(zone, group, pw) {
        g.set_temp_group(zone, inv.sender, group);
        message(zone, inv.sender, &format!("You are now in group {group}."));
    } else {
        message(zone, inv.sender, "Bad password for group.");
    }
}

fn c_setcm(zone: &mut Zone, inv: &Invocation<'_>) {
    use super::chat::mask_type::*;
    let names: [(&str, u8); 9] = [
        ("pub", PUB),
        ("pubmacro", PUB_MACRO),
        ("freq", FREQ),
        ("nmefreq", NMEFREQ),
        ("priv", PRIV),
        ("chat", CHAT),
        ("modchat", MODCHAT),
        ("bcmd", BCOMMAND),
        ("rpriv", REMOTE_PRIV),
    ];
    let mut mask = match inv.target {
        Target::Player(t) => super::chat::player_chat_mask(zone, t),
        _ => 0,
    };
    let mut timeout = 0u64;
    for w in inv.params.split_whitespace() {
        if let Some(t) = w.strip_prefix("-t") {
            timeout = t.trim_start_matches('=').parse().unwrap_or(0);
            continue;
        }
        let (on, name) = match w.as_bytes().first() {
            Some(b'-') => (true, &w[1..]),
            Some(b'+') => (false, &w[1..]),
            _ => continue,
        };
        if let Some((_, bit)) = names.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            if on {
                mask |= 1 << bit;
            } else {
                mask &= !(1 << bit);
            }
        }
    }
    match inv.target {
        Target::Player(t) => super::chat::set_player_chat_mask(zone, t, mask, timeout),
        Target::Arena(a) => super::chat::set_arena_chat_mask(zone, a, mask),
        _ => {}
    }
    message(zone, inv.sender, &format!("Chat mask set to {mask:#06x}."));
}

fn c_getcm(zone: &mut Zone, inv: &Invocation<'_>) {
    let mask = match inv.target {
        Target::Player(t) => super::chat::player_chat_mask(zone, t),
        _ => 0,
    };
    message(zone, inv.sender, &format!("Chat mask: {mask:#06x}"));
}

/// The `staffcmd` module (with ASSS name `playercmd` handled by
/// [`super::playercmd`]).
pub struct StaffCmdModule;

type Handler = fn(&mut Zone, &Invocation<'_>);

const COMMANDS: &[(&str, Handler, &str)] = &[
    (
        "a",
        c_a,
        "Args: <text>\nSends a green message to the target(s), signed with your name.",
    ),
    (
        "aa",
        c_aa,
        "Args: <text>\nSends an anonymous green message to the target(s).",
    ),
    (
        "z",
        c_z,
        "Args: <text>\nSends a green message to the whole zone, signed with your name.",
    ),
    (
        "az",
        c_az,
        "Args: <text>\nSends an anonymous green message to the whole zone.",
    ),
    (
        "warn",
        c_warn,
        "Targets: player\nArgs: <text>\nSends a red warning message to a player.",
    ),
    (
        "arena",
        c_arena,
        "Args: [-a]\nLists the arenas (-a includes empty ones).",
    ),
    (
        "uptime",
        c_uptime,
        "Args: none\nShows how long the server has been running.",
    ),
    (
        "info",
        c_info,
        "Targets: player\nArgs: none\nShows information about a player.",
    ),
    (
        "where",
        c_where,
        "Targets: player or none\nArgs: none\nShows where a player (or you) is.",
    ),
    (
        "find",
        c_find,
        "Args: <name>\nTells you which arena a player is in.",
    ),
    ("lsmod", c_lsmod, "Args: none\nLists the loaded modules."),
    (
        "listmod",
        c_listmod,
        "Args: none\nLists staff members who are online.",
    ),
    (
        "mapinfo",
        c_mapinfo,
        "Args: none\nShows the arena's map file and checksum.",
    ),
    (
        "setship",
        c_setship,
        "Targets: player, freq, or arena\nArgs: <ship 1-8, 0 = spec>\nChanges ship.",
    ),
    (
        "setfreq",
        c_setfreq,
        "Targets: player, freq, or arena\nArgs: <freq>\nChanges frequency.",
    ),
    (
        "specall",
        c_specall,
        "Targets: player, freq, or arena\nArgs: none\nPuts players in spectator mode.",
    ),
    (
        "warpto",
        c_warpto,
        "Targets: player, freq, or arena\nArgs: <x> <y>\nWarps players to a tile.",
    ),
    (
        "shipreset",
        c_shipreset,
        "Targets: player, freq, or arena\nArgs: none\nResets ships (energy, items).",
    ),
    (
        "prize",
        c_prize,
        "Targets: player, freq, or arena\nArgs: [count] <prize>\nGives prizes (negative removes).",
    ),
    (
        "lock",
        c_lock,
        "Targets: player, freq, or arena\nArgs: [-s]\nLocks players in their ship (-s: in spec).",
    ),
    (
        "unlock",
        c_unlock,
        "Targets: player, freq, or arena\nArgs: none\nUnlocks ships.",
    ),
    (
        "send",
        c_send,
        "Targets: player\nArgs: <arena>\nSends a player to an arena.",
    ),
    ("shutdown", c_shutdown, "Args: none\nShuts the server down."),
    (
        "reloadconf",
        c_reloadconf,
        "Args: none\nRe-reads changed config files.",
    ),
    (
        "flagreset",
        c_flagreset,
        "Args: none\nResets the flag game.",
    ),
    (
        "ballcount",
        c_ballcount,
        "Args: [count]\nShows or sets the number of balls.",
    ),
    ("jackpot", c_jackpot, "Args: none\nShows the arena jackpot."),
    (
        "setjackpot",
        c_setjackpot,
        "Args: <points>\nSets the arena jackpot.",
    ),
    (
        "scorereset",
        c_scorereset,
        "Targets: player or none\nArgs: none\nResets scores.",
    ),
    (
        "points",
        c_points,
        "Targets: player, freq, or arena\nArgs: <points>\nGives flag points.",
    ),
    (
        "getgroup",
        c_getgroup,
        "Targets: player or none\nArgs: none\nShows a player's group.",
    ),
    (
        "setgroup",
        c_setgroup,
        "Targets: player\nArgs: [-a] [-t] <group>\nSets a player's group (-a global, -t temporary).",
    ),
    (
        "rmgroup",
        c_rmgroup,
        "Targets: player\nArgs: none\nPuts a player back in the default group.",
    ),
    (
        "grplogin",
        c_grplogin,
        "Args: <group> <password>\nJoins a group with its password.",
    ),
    (
        "setcm",
        c_setcm,
        "Targets: player or arena\nArgs: [-t=<seconds>] -pub +freq ...\nSets a chat mask.",
    ),
    (
        "getcm",
        c_getcm,
        "Targets: player\nArgs: none\nShows a player's chat mask.",
    ),
];

impl Module for StaffCmdModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        for &(name, f, help) in COMMANDS {
            let help = format!("Module: staffcmd\n{help}\n");
            zone.add_command(me, name, None, Some(&help), f);
        }
        Ok(())
    }
}
