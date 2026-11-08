//! `auth_ban`: bans (ASSS `auth_ban.c`, extended).
//!
//! ASSS's commands work as they do there:
//! - `?kick [minutes]` kicks a player (you must be in a higher group) and, with
//!   a time, bans their machine id for that long;
//! - `?listmidbans` and `?delmidban <machine id>`.
//!
//! On top of that, bans are kept in SQLite (`Ban:Database`, default
//! `data/bans.db` in the zone directory; in memory for zones without one) and
//! can match more than the machine id:
//! - `?ban [-i] [-t minutes] [reason]` bans a player by name, machine id and,
//!   when the connection proved one, **identity key**; `-i` adds their IP
//!   address (off by default, because addresses are shared);
//! - `?banlist` and `?unban <id>`.
//!
//! The identity key (`Player::identity_key`) is the client's long-term
//! public key, for protocols that prove possession of one (OpenSec
//! `CLIENT_AUTH`). Only keys the connection has proven are used; a key a
//! client merely claims is never matched, counted or logged, since identity
//! keys are public and anyone could claim someone else's.
//!
//! **A key ban is a speed bump, not an identity system.** It stops someone
//! returning casually with the same installation under a new name, but a
//! client can always present a fresh key (or none) by design, just as it can
//! change name or address. Treat it like a machine-id ban.
//!
//! Bans are checked before any authentication (a [`LoginGate`]), so they hold
//! with the password file and with a biller.

use std::net::SocketAddr;

use rusqlite::{Connection, params};

use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::player::{ClientKind, PlayerId};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::chat::{message, wrapped_message};
use super::core::{AuthData, LoginGate, LoginRequest, auth_code};

/// One ban.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ban {
    /// Ban number.
    pub id: i64,
    /// Name, matched case-insensitively.
    pub name: Option<String>,
    /// IP address.
    pub ip: Option<String>,
    /// Machine id.
    pub machine_id: Option<u32>,
    /// Identity key.
    pub key: Option<[u8; 32]>,
    /// Unix time it ends, or `None` for permanent.
    pub expires: Option<i64>,
    /// Why.
    pub reason: String,
    /// Who banned.
    pub by: String,
}

struct BanDb(Connection);

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn open(zone: &Zone) -> rusqlite::Result<Connection> {
    let db = match zone.dir.as_ref() {
        Some(d) => {
            let rel = zone
                .cfg
                .global()
                .get("Ban", "Database")
                .unwrap_or("data/bans.db")
                .to_owned();
            let path = d.root().join(rel);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Connection::open(path)?
        }
        None => Connection::open_in_memory()?,
    };
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS bans (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT COLLATE NOCASE,
            ip TEXT,
            mid INTEGER,
            key BLOB,
            expires INTEGER,
            reason TEXT NOT NULL DEFAULT '',
            by TEXT NOT NULL DEFAULT '',
            created INTEGER NOT NULL,
            hits INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    Ok(db)
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Ban> {
    let key: Option<Vec<u8>> = r.get(4)?;
    Ok(Ban {
        id: r.get(0)?,
        name: r.get(1)?,
        ip: r.get(2)?,
        machine_id: r.get::<_, Option<i64>>(3)?.map(|m| m as u32),
        key: key.and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok()),
        expires: r.get(5)?,
        reason: r.get(6)?,
        by: r.get(7)?,
    })
}

const COLS: &str = "id, name, ip, mid, key, expires, reason, by";

/// Adds a ban; returns its number.
pub fn add(zone: &Zone, ban: &Ban) -> Option<i64> {
    let db = &zone.res.get::<BanDb>()?.0;
    let r = db.execute(
        "INSERT INTO bans (name, ip, mid, key, expires, reason, by, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            ban.name,
            ban.ip,
            ban.machine_id.map(i64::from),
            ban.key.map(|k| k.to_vec()),
            ban.expires,
            ban.reason,
            ban.by,
            unix_now()
        ],
    );
    match r {
        Ok(_) => Some(db.last_insert_rowid()),
        Err(e) => {
            tracing::warn!(target: "auth_ban", "saving a ban failed: {e}");
            None
        }
    }
}

/// Removes a ban by number; false if there was none.
pub fn remove(zone: &Zone, id: i64) -> bool {
    zone.res
        .get::<BanDb>()
        .and_then(|d| {
            d.0.execute("DELETE FROM bans WHERE id = ?1", params![id])
                .ok()
        })
        .is_some_and(|n| n > 0)
}

/// Bans in force.
pub fn list(zone: &Zone) -> Vec<Ban> {
    let Some(db) = zone.res.get::<BanDb>() else {
        return Vec::new();
    };
    let q = format!("SELECT {COLS} FROM bans WHERE expires IS NULL OR expires > ?1 ORDER BY id");
    let Ok(mut st) = db.0.prepare(&q) else {
        return Vec::new();
    };
    st.query_map(params![unix_now()], row)
        .map(|r| r.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// The ban matching these identifiers, if any is in force.
pub fn find(
    zone: &Zone,
    name: &str,
    ip: Option<&str>,
    machine_id: Option<u32>,
    key: Option<&[u8; 32]>,
) -> Option<Ban> {
    let db = &zone.res.get::<BanDb>()?.0;
    let q = format!(
        "SELECT {COLS} FROM bans WHERE (expires IS NULL OR expires > ?1) AND
           (name = ?2 OR (ip IS NOT NULL AND ip = ?3) OR (mid IS NOT NULL AND mid = ?4)
            OR (key IS NOT NULL AND key = ?5))
         ORDER BY id LIMIT 1"
    );
    db.query_row(
        &q,
        params![
            unix_now(),
            name,
            ip,
            machine_id.map(i64::from),
            key.map(|k| k.to_vec())
        ],
        row,
    )
    .ok()
}

fn ip_of(addr: Option<SocketAddr>) -> Option<String> {
    addr.map(|a| a.ip().to_string())
}

struct Gate;

impl LoginGate for Gate {
    fn check(&self, zone: &mut Zone, p: PlayerId, login: &LoginRequest) -> Option<AuthData> {
        let pl = zone.players.get(p)?;
        // like ASSS, machine ids only count for real game clients
        let standard = matches!(pl.kind, ClientKind::Vie | ClientKind::Continuum);
        let ip = ip_of(pl.addr);
        let key = pl.identity_key;
        let ban = find(
            zone,
            &login.name,
            ip.as_deref(),
            standard.then_some(login.machine_id),
            key.as_ref(),
        )?;
        if let Some(db) = zone.res.get::<BanDb>() {
            let _ = db.0.execute(
                "UPDATE bans SET hits = hits + 1 WHERE id = ?1",
                params![ban.id],
            );
        }
        let left = ban
            .expires
            .map(|e| format!("{} more minutes", (e - unix_now() + 59) / 60))
            .unwrap_or_else(|| "permanently".to_owned());
        tracing::info!(target: "auth_ban", name = %login.name, ban = ban.id, "banned player tried to log in ({left})");
        let mut text = format!("You are banned ({left})");
        if !ban.reason.is_empty() {
            text.push_str(": ");
            text.push_str(&ban.reason);
        }
        // Continuum and chat clients show the reason; core turns this into
        // "locked out" for VIE clients, which can't
        Some(AuthData {
            code: auth_code::CUSTOM_TEXT,
            custom_text: text,
            ..AuthData::default()
        })
    }
}

fn target_player(zone: &mut Zone, inv: &Invocation<'_>) -> Option<PlayerId> {
    match inv.target {
        Target::Player(t) => Some(t),
        _ => {
            message(zone, inv.sender, "Only valid target is a single player");
            None
        }
    }
}

fn may_act_on(zone: &mut Zone, inv: &Invocation<'_>, t: PlayerId, what: &str) -> bool {
    if t == inv.sender {
        return false;
    }
    let higher = zone
        .iface::<dyn super::capman::Capabilities>()
        .is_some_and(|c| c.higher_than(zone, inv.sender, t));
    if !higher {
        let me = zone
            .players
            .get(inv.sender)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        message(
            zone,
            inv.sender,
            &format!("You don't have permission to use ?{what} on that player."),
        );
        message(zone, t, &format!("{me} tried to use ?{what} on you."));
        return false;
    }
    true
}

fn sender_name(zone: &Zone, inv: &Invocation<'_>) -> String {
    zone.players
        .get(inv.sender)
        .map(|p| p.name.clone())
        .unwrap_or_default()
}

/// ASSS `?kick [minutes]`: with minutes, bans the machine id (and identity
/// key, when there is one) for that long.
fn c_kick(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(t) = target_player(zone, inv) else {
        return;
    };
    if !may_act_on(zone, inv, t, "kick") {
        return;
    }
    let mins: i64 = inv
        .params
        .split_whitespace()
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if let Some(p) = zone.players.get(t)
        && mins > 0
        && matches!(p.kind, ClientKind::Vie | ClientKind::Continuum)
    {
        let ban = Ban {
            id: 0,
            name: None,
            ip: None,
            machine_id: Some(p.machine_id),
            key: p.identity_key,
            expires: Some(unix_now() + mins * 60),
            reason: String::new(),
            by: sender_name(zone, inv),
        };
        add(zone, &ban);
    }
    super::core::kick(zone, t);
}

/// `?ban [-i] [-t minutes] [reason]`.
fn c_ban(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(t) = target_player(zone, inv) else {
        return;
    };
    if !may_act_on(zone, inv, t, "ban") {
        return;
    }
    let mut with_ip = false;
    let mut mins: Option<i64> = None;
    let mut words = inv.params.split_whitespace().peekable();
    let mut reason = Vec::new();
    while let Some(w) = words.next() {
        match w {
            "-i" if reason.is_empty() => with_ip = true,
            "-t" if reason.is_empty() => mins = words.next().and_then(|m| m.parse().ok()),
            _ => reason.push(w),
        }
    }
    let Some(p) = zone.players.get(t) else { return };
    let standard = matches!(p.kind, ClientKind::Vie | ClientKind::Continuum);
    let ban = Ban {
        id: 0,
        name: Some(p.name.clone()),
        ip: if with_ip { ip_of(p.addr) } else { None },
        machine_id: standard.then_some(p.machine_id),
        key: p.identity_key,
        expires: mins.filter(|m| *m > 0).map(|m| unix_now() + m * 60),
        reason: reason.join(" "),
        by: sender_name(zone, inv),
    };
    let name = p.name.clone();
    match add(zone, &ban) {
        Some(id) => {
            let what = [
                Some("name"),
                ban.machine_id.map(|_| "machine id"),
                ban.ip.as_ref().map(|_| "IP"),
                ban.key.map(|_| "identity key"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            tracing::info!(target: "auth_ban", ban = id, %name, by = %ban.by, "banned ({what})");
            message(zone, inv.sender, &format!("Ban {id}: {name} ({what})"));
            super::core::kick(zone, t);
        }
        None => message(zone, inv.sender, "The ban couldn't be saved."),
    }
}

fn describe(b: &Ban) -> String {
    let mut parts = Vec::new();
    if let Some(n) = &b.name {
        parts.push(n.clone());
    }
    if let Some(m) = b.machine_id {
        parts.push(format!("mid {m}"));
    }
    if let Some(ip) = &b.ip {
        parts.push(ip.clone());
    }
    if let Some(k) = b.key {
        let hex: String = k[..4].iter().map(|x| format!("{x:02x}")).collect();
        parts.push(format!("key {hex}…"));
    }
    let left = b
        .expires
        .map(|e| format!("{} min. left", (e - unix_now() + 30) / 60))
        .unwrap_or_else(|| "permanent".to_owned());
    format!("{}: {} ({left})", b.id, parts.join(" "))
}

fn c_banlist(zone: &mut Zone, inv: &Invocation<'_>) {
    let bans = list(zone);
    if bans.is_empty() {
        message(zone, inv.sender, "No bans in force.");
    }
    for b in bans {
        message(zone, inv.sender, &describe(&b));
    }
}

fn c_unban(zone: &mut Zone, inv: &Invocation<'_>) {
    let text = match inv.params.trim().parse::<i64>() {
        Ok(id) if remove(zone, id) => format!("Ban {id} removed."),
        _ => "No such ban.".to_owned(),
    };
    message(zone, inv.sender, &text);
}

/// ASSS `?listmidbans`.
fn c_listmidbans(zone: &mut Zone, inv: &Invocation<'_>) {
    let mids: Vec<String> = list(zone)
        .into_iter()
        .filter(|b| b.name.is_none() && b.machine_id.is_some())
        .map(|b| match b.expires {
            Some(e) => format!(
                "{} ({} min. left)",
                b.machine_id.unwrap_or(0),
                (e - unix_now() + 30) / 60
            ),
            None => b.machine_id.unwrap_or(0).to_string(),
        })
        .collect();
    message(zone, inv.sender, "Active machine id bans:");
    wrapped_message(zone, inv.sender, &mids.join(", "));
}

/// ASSS `?delmidban`.
fn c_delmidban(zone: &mut Zone, inv: &Invocation<'_>) {
    match inv.params.trim().parse::<u32>() {
        Ok(mid) if mid != 0 => {
            if let Some(db) = zone.res.get::<BanDb>() {
                let _ = db.0.execute(
                    "DELETE FROM bans WHERE mid = ?1 AND name IS NULL",
                    params![i64::from(mid)],
                );
            }
        }
        _ => message(zone, inv.sender, "Invalid machine id."),
    }
}

/// The `auth_ban` module.
pub struct BansModule;

impl Module for BansModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let db = open(zone).map_err(|e| ModuleError::Failed {
            module: "auth_ban".into(),
            reason: e.to_string(),
        })?;
        zone.res.insert(BanDb(db));
        zone.reg
            .provide::<dyn LoginGate>(me, Scope::Global, std::sync::Arc::new(Gate));
        let cmds: [(&str, crate::commands::CommandFn, &str); 6] = [
            (
                "kick",
                std::sync::Arc::new(c_kick),
                "Module: auth_ban\nTargets: player\nArgs: [<timeout>]\nKicks the player off of the server, with an optional timeout (in minutes).\n",
            ),
            (
                "ban",
                std::sync::Arc::new(c_ban),
                "Module: auth_ban\nTargets: player\nArgs: [-i] [-t <minutes>] [<reason>]\nBans the player by name, machine id and identity key (-i: also IP address),\npermanently or for a time, and kicks them.\n",
            ),
            (
                "banlist",
                std::sync::Arc::new(c_banlist),
                "Module: auth_ban\nTargets: none\nArgs: none\nLists the bans in force.\n",
            ),
            (
                "unban",
                std::sync::Arc::new(c_unban),
                "Module: auth_ban\nTargets: none\nArgs: <ban number>\nRemoves a ban.\n",
            ),
            (
                "listmidbans",
                std::sync::Arc::new(c_listmidbans),
                "Module: auth_ban\nTargets: none\nArgs: none\nLists the current machine id bans in effect.\n",
            ),
            (
                "delmidban",
                std::sync::Arc::new(c_delmidban),
                "Module: auth_ban\nTargets: none\nArgs: <machine id>\nRemoves a machine id ban.\n",
            ),
        ];
        for (name, f, help) in cmds {
            zone.commands.add(me, name, None, Some(help), f);
        }
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<BanDb>();
    }
}
