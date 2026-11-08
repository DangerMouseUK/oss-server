//! `persist`: persistent player data in SQLite (ASSS `scoring/persist.c`,
//! which used Berkeley DB).
//!
//! Modules register [`PlayerData`] slots (a key, an interval and a scope) with
//! functions that produce and consume a byte blob. The core asks this module
//! (through [`super::core::Persist`]) to load a player's data when they log in
//! and enter an arena, and to save it when they leave; data is also saved
//! every `Persist:SyncSeconds` (default 180). Data belongs to an interval
//! (forever, reset, game, ...); [`end_interval`] starts a new period for an
//! arena group, so per-game stats start over after a flag or soccer win.
//!
//! The database is `data/oss.db` in the zone directory (`Persist:Database`
//! overrides the path), or in memory for zones without a directory.
//! Database access runs on the game thread; it is a local file in WAL mode
//! and only touched on arena changes and every few minutes.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::core::{Persist, player_sync_done};

/// ASSS persistence intervals (`INTERVAL_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interval {
    /// Never resets.
    Forever = 0,
    /// Reset by staff (`?scorereset`).
    Reset = 1,
    /// Per map rotation.
    MapRotation = 2,
    /// Per flag/soccer game.
    Game = 5,
    /// Forever, not shared between arenas of a group.
    ForeverNonshared = 6,
}

/// Where a data slot applies (ASSS `PERSIST_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistScope {
    /// Separate data per arena group.
    PerArena,
    /// One value zone-wide.
    Global,
}

/// Produces a player's blob (or `None` to store nothing).
pub type GetFn = Arc<dyn Fn(&Zone, PlayerId, Option<ArenaId>) -> Option<Vec<u8>> + Send + Sync>;
/// Restores a player's blob.
pub type SetFn = Arc<dyn Fn(&mut Zone, PlayerId, Option<ArenaId>, &[u8]) + Send + Sync>;
/// Clears a player's data (nothing stored, or a new interval).
pub type ClearFn = Arc<dyn Fn(&mut Zone, PlayerId, Option<ArenaId>) + Send + Sync>;

/// A persistent player data slot (ASSS `PlayerPersistentData`).
#[derive(Clone)]
pub struct PlayerData {
    /// Key distinguishing this slot (ASSS `KEY_*`).
    pub key: u32,
    /// Interval.
    pub interval: Interval,
    /// Scope.
    pub scope: PersistScope,
    /// Producer.
    pub get: GetFn,
    /// Consumer.
    pub set: SetFn,
    /// Clearer.
    pub clear: ClearFn,
}

#[derive(Default)]
struct Registry(Vec<(Owner, PlayerData)>);

struct Db(Connection);

/// ASSS `CB_INTERVAL_ENDED`.
#[derive(Debug, Clone, Copy)]
pub struct IntervalEnded {
    /// Arena group the interval ended for (`None` = all).
    pub arena: Option<ArenaId>,
    /// Interval.
    pub interval: Interval,
}

impl Event for IntervalEnded {
    fn arena(&self) -> Option<ArenaId> {
        self.arena
    }
}

/// Registers a player data slot (call from a module's `load`).
pub fn register_player_data(zone: &mut Zone, owner: Owner, data: PlayerData) {
    zone.res.get_or_default::<Registry>().0.push((owner, data));
}

/// Removes an owner's slots.
pub fn unregister_player_data(zone: &mut Zone, owner: Owner) {
    if let Some(r) = zone.res.get_mut::<Registry>() {
        r.0.retain(|(o, _)| *o != owner);
    }
}

const GLOBAL_GROUP: &str = "(global)";

fn group(zone: &Zone, arena: Option<ArenaId>) -> String {
    match arena.and_then(|a| zone.arenas.get(a)) {
        Some(a) => a.basename.clone(),
        None => GLOBAL_GROUP.to_owned(),
    }
}

fn serial(db: &Connection, grp: &str, interval: Interval) -> i64 {
    db.query_row(
        "SELECT serial FROM serials WHERE arenagrp = ?1 AND interval = ?2",
        params![grp, interval as i64],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(0)
}

fn slots(zone: &Zone, arena: Option<ArenaId>) -> Vec<PlayerData> {
    let want = if arena.is_some() {
        PersistScope::PerArena
    } else {
        PersistScope::Global
    };
    slots_in(zone, want)
}

fn slots_in(zone: &Zone, want: PersistScope) -> Vec<PlayerData> {
    zone.res
        .get::<Registry>()
        .map(|r| {
            r.0.iter()
                .filter(|(_, d)| d.scope == want)
                .map(|(_, d)| d.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn load_player(zone: &mut Zone, pid: PlayerId, arena: Option<ArenaId>) {
    let Some(name) = zone.players.get(pid).map(|p| p.name.to_ascii_lowercase()) else {
        return;
    };
    let grp = group(zone, arena);
    for slot in slots(zone, arena) {
        let blob: Option<Vec<u8>> = zone.res.get::<Db>().and_then(|db| {
            let s = serial(&db.0, &grp, slot.interval);
            db.0.query_row(
                "SELECT data FROM player_data WHERE name = ?1 AND arenagrp = ?2 AND interval = ?3 AND serial = ?4 AND key = ?5",
                params![name, grp, slot.interval as i64, s, i64::from(slot.key)],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
        });
        (slot.clear)(zone, pid, arena);
        if let Some(b) = blob {
            (slot.set)(zone, pid, arena, &b);
        }
    }
}

fn save_player(zone: &mut Zone, pid: PlayerId, arena: Option<ArenaId>) {
    let Some(name) = zone.players.get(pid).map(|p| p.name.to_ascii_lowercase()) else {
        return;
    };
    let grp = group(zone, arena);
    for slot in slots(zone, arena) {
        let blob = (slot.get)(zone, pid, arena);
        let Some(db) = zone.res.get::<Db>() else {
            return;
        };
        let s = serial(&db.0, &grp, slot.interval);
        let r = match blob {
            Some(b) => db.0.execute(
                "INSERT INTO player_data (name, arenagrp, interval, serial, key, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (name, arenagrp, interval, serial, key) DO UPDATE SET data = excluded.data",
                params![name, grp, slot.interval as i64, s, i64::from(slot.key), b],
            ),
            None => db.0.execute(
                "DELETE FROM player_data WHERE name = ?1 AND arenagrp = ?2 AND interval = ?3 AND serial = ?4 AND key = ?5",
                params![name, grp, slot.interval as i64, s, i64::from(slot.key)],
            ),
        };
        if let Err(e) = r {
            tracing::warn!(target: "persist", pid = pid.0, "saving player data failed: {e}");
        }
    }
}

/// ASSS `EndInterval`: starts a new period of `interval` for an arena's group
/// (or every group with `None`); players there have that data cleared.
pub fn end_interval(zone: &mut Zone, arena: Option<ArenaId>, interval: Interval) {
    let grp = arena.map(|a| group(zone, Some(a)));
    // save current data first so the ended period keeps its final values
    let affected: Vec<(PlayerId, ArenaId)> = zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing)
        .filter_map(|p| p.arena.map(|a| (p.id, a)))
        .filter(|(_, a)| arena.is_none() || grp.as_deref() == Some(group(zone, Some(*a)).as_str()))
        .collect();
    for (pid, a) in &affected {
        save_player(zone, *pid, Some(*a));
    }
    if let Some(db) = zone.res.get::<Db>() {
        let r = match &grp {
            Some(g) => db.0.execute(
                "INSERT INTO serials (arenagrp, interval, serial) VALUES (?1, ?2, 1)
                 ON CONFLICT (arenagrp, interval) DO UPDATE SET serial = serial + 1",
                params![g, interval as i64],
            ),
            None => db.0.execute(
                "UPDATE serials SET serial = serial + 1 WHERE interval = ?1",
                params![interval as i64],
            ),
        };
        if let Err(e) = r {
            tracing::warn!(target: "persist", "ending interval failed: {e}");
        }
    }
    for slot in slots_in(zone, PersistScope::PerArena)
        .into_iter()
        .filter(|s| s.interval == interval)
    {
        for (pid, a) in &affected {
            (slot.clear)(zone, *pid, Some(*a));
        }
    }
    zone.fire(&IntervalEnded { arena, interval });
}

fn open(zone: &Zone) -> rusqlite::Result<Connection> {
    let configured = zone
        .cfg
        .global()
        .get("Persist", "Database")
        .map(str::to_owned);
    let conn = match (zone.dir.as_ref(), configured) {
        (Some(d), cfg) => {
            let path = d
                .root()
                .join(cfg.unwrap_or_else(|| "data/oss.db".to_owned()));
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let c = Connection::open(path)?;
            c.pragma_update(None, "journal_mode", "WAL")?;
            c
        }
        (None, _) => Connection::open_in_memory()?,
    };
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS player_data (
            name TEXT NOT NULL, arenagrp TEXT NOT NULL, interval INTEGER NOT NULL,
            serial INTEGER NOT NULL, key INTEGER NOT NULL, data BLOB NOT NULL,
            PRIMARY KEY (name, arenagrp, interval, serial, key));
         CREATE TABLE IF NOT EXISTS serials (
            arenagrp TEXT NOT NULL, interval INTEGER NOT NULL, serial INTEGER NOT NULL,
            PRIMARY KEY (arenagrp, interval));",
    )?;
    Ok(conn)
}

/// The persist implementation of [`Persist`].
pub struct PersistImpl;

impl Persist for PersistImpl {
    fn get_player(&self, zone: &mut Zone, player: PlayerId, arena: Option<ArenaId>) {
        load_player(zone, player, arena);
        player_sync_done(zone, player);
    }

    fn put_player(&self, zone: &mut Zone, player: PlayerId, arena: Option<ArenaId>) {
        save_player(zone, player, arena);
        player_sync_done(zone, player);
    }
}

/// The `persist` module.
pub struct PersistModule;

impl Module for PersistModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let conn = open(zone).map_err(|e| ModuleError::Failed {
            module: "persist".into(),
            reason: e.to_string(),
        })?;
        zone.res.insert(Db(conn));
        zone.reg
            .provide::<dyn Persist>(me, Scope::Global, Arc::new(PersistImpl));
        let secs = u64::try_from(zone.cfg.global().get_int("Persist", "SyncSeconds", 180))
            .unwrap_or(180)
            .max(10);
        zone.set_timer(me, secs * 100, secs * 100, |z| {
            let playing: Vec<(PlayerId, ArenaId)> = z
                .players
                .iter()
                .filter(|p| p.status == PlayerStatus::Playing)
                .filter_map(|p| p.arena.map(|a| (p.id, a)))
                .collect();
            for (pid, a) in playing {
                save_player(z, pid, Some(a));
                save_player(z, pid, None);
            }
            true
        });
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<Db>();
    }
}
