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
        .is_some_and(|c| c.has_capability(zone, inv.sender, super::capman::CAP_SEE_PRIV_ARENA));
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

fn c_recyclearena(zone: &mut Zone, inv: &Invocation<'_>) {
    let ok = inv
        .arena
        .is_some_and(|a| super::arenaman::recycle_arena(zone, a));
    if !ok {
        message(
            zone,
            inv.sender,
            "Arena recycle failed; check the log for details.",
        );
    }
}

fn c_endinterval(zone: &mut Zone, inv: &Invocation<'_>) {
    use super::persist::Interval;
    let (mut interval, mut group, mut want_group) = (None, None, false);
    for word in inv.params.split_whitespace() {
        if want_group {
            group = Some(word.to_owned());
            want_group = false;
            continue;
        }
        match word {
            "-g" => group = Some(String::new()),
            "-a" => want_group = true,
            "game" => interval = Some(Interval::Game),
            "reset" => interval = Some(Interval::Reset),
            "maprotation" => interval = Some(Interval::MapRotation),
            _ => {
                message(zone, inv.sender, &format!("Bad argument: {word}"));
                return;
            }
        }
    }
    if want_group {
        message(
            zone,
            inv.sender,
            "You must specify an arena group name after -a.",
        );
        return;
    }
    let Some(interval) = interval else {
        message(zone, inv.sender, "You must specify an interval to reset.");
        return;
    };
    match group {
        // -g: the global scope, every group at once
        Some(g) if g.is_empty() => super::persist::end_interval(zone, None, interval),
        Some(g) => super::persist::end_named_interval(zone, &g, interval),
        None => {
            if let Some(a) = inv.arena {
                super::persist::end_interval(zone, Some(a), interval);
            }
        }
    }
}

fn c_zone(zone: &mut Zone, inv: &Invocation<'_>) {
    let name = zone
        .cfg
        .global()
        .get("Billing", "ServerName")
        .unwrap_or("(none)")
        .to_owned();
    message(zone, inv.sender, &format!("zone: {name}"));
}

fn c_owner(zone: &mut Zone, inv: &Invocation<'_>) {
    let owner = inv
        .arena
        .and_then(|a| zone.arenas.get(a))
        .and_then(|a| a.cfg.get("Owner", "Name"))
        .unwrap_or("(none)")
        .to_owned();
    message(zone, inv.sender, &format!("arena owner: {owner}"));
}

fn c_listarena(zone: &mut Zone, inv: &Invocation<'_>) {
    let name = inv.params.trim();
    if name.starts_with('#')
        && !zone
            .iface::<dyn super::capman::Capabilities>()
            .is_some_and(|c| c.has_capability(zone, inv.sender, super::capman::CAP_SEE_PRIV_ARENA))
    {
        message(
            zone,
            inv.sender,
            "You don't have permission to view private arenas.",
        );
        return;
    }
    let arena = if name.is_empty() {
        inv.arena
    } else {
        zone.arenas.iter().find(|a| a.name == name).map(|a| a.id)
    };
    let Some(arena) = arena else {
        message(zone, inv.sender, &format!("Arena '{name}' doesn't exist."));
        return;
    };
    let arena_name = zone
        .arenas
        .get(arena)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let mut names = Vec::new();
    let mut playing = 0;
    for p in zone.players.in_arena(arena) {
        if p.status == PlayerStatus::Playing {
            if !p.ship.is_spec() {
                playing += 1;
            }
            names.push(p.name.clone());
        }
    }
    message(
        zone,
        inv.sender,
        &format!(
            "Arena '{arena_name}': {} total, {playing} playing",
            names.len()
        ),
    );
    for line in wrapped(&names) {
        message(zone, inv.sender, &line);
    }
}

/// ASSS `SendWrappedText`: comma-separated names, at most 80 characters a
/// line.
fn wrapped(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        match out.last_mut() {
            Some(l) if l.len() + n.len() + 2 <= 80 => {
                l.push_str(", ");
                l.push_str(n);
            }
            _ => out.push(n.clone()),
        }
    }
    out
}

fn c_modinfo(zone: &mut Zone, inv: &Invocation<'_>) {
    let name = inv.params.trim();
    let Some(canon) = zone.modules.catalog().canonical(name) else {
        message(
            zone,
            inv.sender,
            &format!("No information for module '{name}'"),
        );
        return;
    };
    let aliases = zone.modules.catalog().aliases(&canon);
    let loaded = zone.modules.is_loaded(&canon);
    let owner = zone.modules.owner_of(&canon);
    let attached: Vec<String> = owner
        .map(|o| {
            zone.arenas
                .iter()
                .filter(|a| a.attached.contains(&o))
                .map(|a| a.name.clone())
                .collect()
        })
        .unwrap_or_default();
    message(zone, inv.sender, &format!("Module: {canon}"));
    if !aliases.is_empty() {
        let list = aliases.join(", ");
        message(zone, inv.sender, &format!("  also known as: {list}"));
    }
    message(
        zone,
        inv.sender,
        &format!("  loaded: {}", if loaded { "yes" } else { "no" }),
    );
    if !attached.is_empty() {
        let list = attached.join(", ");
        message(zone, inv.sender, &format!("  attached to: {list}"));
    }
}

fn c_netstats(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(m) = zone
        .res
        .get::<std::sync::Arc<crate::metrics::Metrics>>()
        .cloned()
    else {
        message(zone, inv.sender, "netstats: no network counters here");
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    let secs = (zone.now().0 / 100).max(1);
    let (dg_in, dg_out) = (m.datagrams_in.load(Relaxed), m.datagrams_out.load(Relaxed));
    let (b_in, b_out) = (m.bytes_in.load(Relaxed), m.bytes_out.load(Relaxed));
    let dropped = m.inbound_dropped.load(Relaxed);
    message(
        zone,
        inv.sender,
        &format!("netstats: pkts sent={dg_out}  pkts recvd={dg_in}  dropped={dropped}"),
    );
    // as in ASSS, 28 bytes of IP and UDP headers count towards bandwidth
    message(
        zone,
        inv.sender,
        &format!(
            "netstats: bw out={}  bw in={}",
            (b_out + dg_out * 28) / secs,
            (b_in + dg_in * 28) / secs
        ),
    );
    let ticks = m.ticks.load(Relaxed).max(1);
    message(
        zone,
        inv.sender,
        &format!(
            "netstats: ticks={ticks}  mean {}us  longest {}us",
            m.tick_ns.load(Relaxed) / ticks / 1000,
            m.tick_ns_max.load(Relaxed) / 1000
        ),
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

fn c_insmod(zone: &mut Zone, inv: &Invocation<'_>) {
    let name = inv.params.trim();
    let text = match crate::module::insert(zone, name) {
        Ok(_) => format!("Module {name} loaded successfully"),
        Err(e) => {
            tracing::info!(target: "staffcmd", "insmod {name}: {e}");
            format!("Loading module {name} failed")
        }
    };
    message(zone, inv.sender, &text);
}

fn c_rmmod(zone: &mut Zone, inv: &Invocation<'_>) {
    let name = inv.params.trim();
    let text = match crate::module::unload(zone, name) {
        Ok(()) => format!("Module {name} unloaded successfully"),
        Err(_) => format!("Unloading module {name} failed"),
    };
    message(zone, inv.sender, &text);
}

fn c_attmod(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let text = match inv.params.strip_prefix("-d") {
        Some(rest) => {
            let name = rest.trim();
            match crate::module::detach(zone, name, a) {
                Ok(()) => format!("Module {name} detached."),
                Err(_) => format!("Detaching module {name} failed."),
            }
        }
        None => {
            let name = inv.params.trim();
            match crate::module::attach(zone, name, a) {
                Ok(()) => format!("Module {name} attached."),
                Err(_) => format!("Attaching module {name} failed."),
            }
        }
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
    for p in &to {
        super::anticheat::excuse(zone, *p, super::anticheat::Excuse::Jump);
    }
}

fn c_shipreset(zone: &mut Zone, inv: &Invocation<'_>) {
    let to = targets(zone, inv);
    send_to(zone, &to, &ShipReset {});
    for p in &to {
        super::anticheat::excuse(zone, *p, super::anticheat::Excuse::Refill);
        super::anticheat::respawned(zone, *p);
    }
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
    for p in &to {
        for _ in 0..count.unsigned_abs().clamp(1, 100) {
            super::anticheat::prize_taken(zone, *p, i32::from(prize));
        }
    }
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

fn c_botfeature(zone: &mut Zone, inv: &Invocation<'_>) {
    for word in inv.params.split([' ', ',']).filter(|w| !w.is_empty()) {
        let (on, name) = match word.split_at_checked(1) {
            Some(("+", rest)) => (true, rest),
            Some(("-", rest)) => (false, rest),
            _ => {
                message(zone, inv.sender, "Bad syntax");
                continue;
            }
        };
        if !super::game::set_bot_feature(zone, inv.sender, name, on) {
            message(zone, inv.sender, "Unknown bot feature");
        }
    }
}

fn c_lockarena(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let p = inv.params;
    super::game::lock_arena(
        zone,
        a,
        p.contains("-n"),
        p.contains("-a"),
        p.contains("-i"),
        p.contains("-s"),
    );
}

fn c_unlockarena(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    super::game::unlock_arena(
        zone,
        a,
        inv.params.contains("-n"),
        inv.params.contains("-a"),
    );
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
    let n = super::arenaman::reload_config(zone);
    message(
        zone,
        inv.sender,
        &format!("Reloaded {n} changed config file(s)."),
    );
}

/// ASSS `Cset_generic`'s parse: an optional `-t`, then `section:key`
/// terminated by `=` or by a second `:`, then the value. Returns
/// (permanent, section, key, value).
fn parse_setting(params: &str) -> Option<(bool, &str, &str, &str)> {
    let mut t = params.trim_start();
    let mut permanent = true;
    if let Some(rest) = t.strip_prefix("-t") {
        permanent = false;
        t = rest.trim_start();
    }
    let (section, rest) = t.split_once(':')?;
    // the key ends at the `=` or at the second `:`, whichever comes first
    let end = rest.find(['=', ':'])?;
    let (key, value) = (&rest[..end], &rest[end + 1..]);
    (!section.is_empty() && !key.is_empty()).then_some((permanent, section, key, value))
}

fn set_generic(zone: &mut Zone, inv: &Invocation<'_>, arena: bool) {
    let Some((permanent, section, key, value)) = parse_setting(inv.params) else {
        message(
            zone,
            inv.sender,
            "Usage: ?setg [-t] section:key=value (no spaces around : or =).",
        );
        return;
    };
    let old = if arena {
        let Some(a) = inv.arena.and_then(|a| zone.arenas.get(a)) else {
            return;
        };
        a.cfg.clone()
    } else {
        zone.cfg.global().clone()
    };
    // ASSS writes "set by <name> on <ctime>"; the server has no date
    // formatter, so the time is unix seconds
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let info = format!("set by {} at {unix}", sender_name(zone, inv));
    let new = zone
        .cfg
        .set(&old, section, key, value, Some(&info), permanent);
    super::arenaman::config_updated(zone, &old, &new);
    let kind = if permanent { "set" } else { "set temporarily" };
    message(
        zone,
        inv.sender,
        &format!("{section}:{key} {kind} to {value}"),
    );
}

fn get_generic(zone: &mut Zone, inv: &Invocation<'_>, arena: bool) {
    let full = inv.params.trim();
    let Some((section, key)) = full.split_once(':') else {
        message(zone, inv.sender, "Usage: ?getg section:key");
        return;
    };
    let file = if arena {
        inv.arena.and_then(|a| zone.arenas.get(a)).map(|a| &a.cfg)
    } else {
        Some(zone.cfg.global())
    };
    let found = file.and_then(|f| f.get(section, key)).map(str::to_owned);
    let text = match found {
        Some(v) => format!("{full}={v}"),
        None => format!("{full} not found"),
    };
    message(zone, inv.sender, &text);
}

fn c_setg(zone: &mut Zone, inv: &Invocation<'_>) {
    set_generic(zone, inv, false);
}

fn c_seta(zone: &mut Zone, inv: &Invocation<'_>) {
    set_generic(zone, inv, true);
}

fn c_getg(zone: &mut Zone, inv: &Invocation<'_>) {
    get_generic(zone, inv, false);
}

fn c_geta(zone: &mut Zone, inv: &Invocation<'_>) {
    get_generic(zone, inv, true);
}

fn c_flagreset(zone: &mut Zone, inv: &Invocation<'_>) {
    if let Some(a) = inv.arena {
        super::flagcore::flag_reset(zone, a, -1, 0);
    }
}

fn c_flaginfo(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    // ASSS prints at most 20
    let n = super::flagcore::flag_count(zone, a).min(20);
    for i in 0..n {
        let Some(f) = super::flagcore::get_flag(zone, a, i) else {
            continue;
        };
        let text = match f.state {
            super::flagcore::FlagState::None => format!("flag {i}: doesn't exist"),
            super::flagcore::FlagState::OnMap => {
                // ASSS's map letter and number: 20 columns across 1024 tiles
                let (cx, cy) = (f.x as i32 * 20 / 1024, f.y as i32 * 20 / 1024);
                let col = char::from(b'A' + (cx.clamp(0, 25) as u8));
                format!(
                    "flag {i}: on the map at {col}{} ({},{}), owned by freq {}",
                    cy + 1,
                    f.x,
                    f.y,
                    f.freq
                )
            }
            super::flagcore::FlagState::Carried => {
                let who = f
                    .carrier
                    .and_then(|c| zone.players.get(c))
                    .map(|p| p.name.clone());
                match who {
                    Some(name) => format!("flag {i}: carried by {name}"),
                    None => continue,
                }
            }
        };
        message(zone, inv.sender, &text);
    }
}

/// A flag id the arena actually has, and whatever follows it.
fn flag_arg(zone: &Zone, inv: &Invocation<'_>) -> Option<(usize, String)> {
    let a = inv.arena?;
    let params = inv.params.trim();
    let (first, rest) = params.split_once(' ').unwrap_or((params, ""));
    let id: usize = first.parse().ok()?;
    (id < super::flagcore::flag_count(zone, a)).then(|| (id, rest.trim().to_owned()))
}

fn c_neutflag(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let Some((id, rest)) = flag_arg(zone, inv) else {
        message(zone, inv.sender, "neutflag: bad flag id");
        return;
    };
    let Some(f) = super::flagcore::get_flag(zone, a, id) else {
        message(zone, inv.sender, "neutflag: bad flag id");
        return;
    };
    // ASSS's undocumented "force" takes a flag off a carrier too
    if f.state == super::flagcore::FlagState::OnMap || rest == "force" {
        super::flagcore::set_flag(zone, a, id, super::flagcore::FlagInfo::default());
    } else {
        message(
            zone,
            inv.sender,
            "neutflag: that flag isn't currently on the map",
        );
    }
}

fn c_moveflag(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let Some((id, rest)) = flag_arg(zone, inv) else {
        message(zone, inv.sender, "moveflag: bad flag id");
        return;
    };
    let Some(mut f) = super::flagcore::get_flag(zone, a, id) else {
        message(zone, inv.sender, "moveflag: bad flag id");
        return;
    };
    if f.state != super::flagcore::FlagState::OnMap {
        message(
            zone,
            inv.sender,
            &format!("moveflag: flag {id} isn't on the map"),
        );
        return;
    }
    let mut words = rest.split([' ', ',']).filter(|w| !w.is_empty());
    let freq: i16 = match words.next().and_then(|w| w.parse().ok()) {
        Some(f) if (0..=9999).contains(&f) => f,
        _ => {
            message(zone, inv.sender, "moveflag: bad freq");
            return;
        }
    };
    f.freq = freq;
    let (x, y) = (
        words.next().and_then(|w| w.parse::<i32>().ok()),
        words.next().and_then(|w| w.parse::<i32>().ok()),
    );
    if let (Some(x), Some(y)) = (x, y)
        && (1..1024).contains(&x)
        && (1..1024).contains(&y)
    {
        let spot = super::mapdata::arena_map(zone, a).map(|m| m.find_empty_tile_near(x, y));
        let (x, y) = spot.unwrap_or((x, y));
        (f.x, f.y) = (x as i16, y as i16);
    }
    super::flagcore::set_flag(zone, a, id, f);
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
        "insmod",
        c_insmod,
        "Targets: none\nArgs: <module name>\nImmediately loads the specified module into the server.",
    ),
    (
        "rmmod",
        c_rmmod,
        "Targets: none\nArgs: <module name>\nAttempts to unload the specified module from the server.",
    ),
    (
        "attmod",
        c_attmod,
        "Targets: none\nArgs: [{-d}] <module name>\nAttaches the specified module to this arena. Or with {-d},\ndetaches the module from the arena.",
    ),
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
        "botfeature",
        c_botfeature,
        "Targets: none\nArgs: [+/-]seeallposn [+/-]seeownposn\nTurns on or off the extra position packets a bot can ask for: every position packet in the arena, and your own echoed back.",
    ),
    (
        "lockarena",
        c_lockarena,
        "Targets: arena\nArgs: [-n] [-a] [-i] [-s]\nLocks the arena, so players entering it are held in spectator mode, and locks everyone already here. -n tells them, -a changes only the arena's own state, -i holds entering players in the ship they arrive in instead of spec, -s specs everyone here first.",
    ),
    (
        "unlockarena",
        c_unlockarena,
        "Targets: arena\nArgs: [-n] [-a]\nUnlocks the arena and everyone in it. -n tells them, -a changes only the arena's own state.",
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
    ("zone", c_zone, "Args: none\nShows the name of this zone."),
    (
        "recyclearena",
        c_recyclearena,
        "Targets: none\nArgs: none\nRecycles the arena: everyone is sent out and straight back in, so the arena is destroyed and built again with its modules reattached. Nobody is kicked off the server.",
    ),
    (
        "endinterval",
        c_endinterval,
        "Targets: none\nArgs: [-g] [-a <arena group>] <game|reset|maprotation>\nEnds a scoring interval, so those scores start again. -g does it everywhere, -a for one arena group, otherwise this arena's.",
    ),
    (
        "owner",
        c_owner,
        "Args: none\nShows the arena owner (`Owner:Name` in its config).",
    ),
    (
        "listarena",
        c_listarena,
        "Targets: none\nArgs: [<arena name>]\nLists the players in an arena, or in this one.",
    ),
    (
        "modinfo",
        c_modinfo,
        "Targets: none\nArgs: <module name>\nShows what the server knows about a module: its ASSS names, whether it is loaded, and the arenas it is attached to.",
    ),
    (
        "netstats",
        c_netstats,
        "Targets: none\nArgs: none\nShows the network and game-loop counters.",
    ),
    (
        "setg",
        c_setg,
        "Targets: none\nArgs: [-t] section:key=value\nSets a global setting. No spaces around the colon or the equals sign. -t makes it temporary (not written to the file).",
    ),
    (
        "seta",
        c_seta,
        "Targets: none\nArgs: [-t] section:key=value\nSets a setting in this arena's config, as ?setg does for the global one.",
    ),
    (
        "getg",
        c_getg,
        "Targets: none\nArgs: section:key\nShows the value of a global setting.",
    ),
    (
        "geta",
        c_geta,
        "Targets: none\nArgs: section:key\nShows the value of a setting in this arena's config.",
    ),
    (
        "flaginfo",
        c_flaginfo,
        "Targets: none\nArgs: none\nShows the state, place and owner of each flag in the arena.",
    ),
    (
        "neutflag",
        c_neutflag,
        "Targets: none\nArgs: <flag id>\nNeuts a flag that is on the map, so the flag timer puts it back in the middle.",
    ),
    (
        "moveflag",
        c_moveflag,
        "Targets: none\nArgs: <flag id> <owning freq> [<x> <y>]\nMoves a flag on the map and gives it to a freq. Without coordinates it stays where it is.",
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
