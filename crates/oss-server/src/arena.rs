//! Arenas: the core per-arena state every module shares (ASSS `Arena`).

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::ConfigFile;
use crate::registry::{Extensions, Owner};

/// An arena handle. Ids are never reused within a server run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArenaId(pub u32);

/// The base name of public arenas (`AG_PUBLIC`).
pub const PUBLIC_BASENAME: &str = "(public)";

/// Arena lifecycle (ASSS `ARENA_*` states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArenaStatus {
    /// Load config and run pre-create callbacks.
    DoInit0,
    /// Waiting on holds from the first round of callbacks.
    WaitHolds0,
    /// Attach modules and run create callbacks.
    DoInit1,
    /// Waiting on modules' init work.
    WaitHolds1,
    /// Load persistent data.
    DoInit2,
    /// Waiting on persistent data.
    WaitSync1,
    /// Running; players can enter.
    Running,
    /// Running but not accepting new players.
    Closing,
    /// Save persistent data before destroying.
    DoWriteData,
    /// Waiting on persistent data.
    WaitSync2,
    /// Destroy callbacks.
    DoDestroy1,
    /// Waiting on modules' destroy work.
    WaitHolds2,
    /// Finish destroying.
    DoDestroy2,
}

/// ASSS arena base name: the name (at most 19 bytes) with trailing digits
/// removed, or [`PUBLIC_BASENAME`] if nothing is left (`"0"` → `(public)`).
pub fn basename(name: &str) -> String {
    let mut end = name.len().min(19);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let base = name[..end].trim_end_matches(|c: char| c.is_ascii_digit());
    if base.is_empty() {
        PUBLIC_BASENAME.to_owned()
    } else {
        base.to_owned()
    }
}

/// One arena.
pub struct Arena {
    /// Handle.
    pub id: ArenaId,
    /// Full name, e.g. `"0"` or `"turf2"`.
    pub name: String,
    /// Base name used to find config and map files, e.g. `"(public)"` or `"turf"`.
    pub basename: String,
    /// Lifecycle state.
    pub status: ArenaStatus,
    /// The arena's config (`arena.conf` and includes).
    pub cfg: Arc<ConfigFile>,
    /// Modules attached to this arena.
    pub attached: Vec<Owner>,
    /// Outstanding holds that delay the next lifecycle step.
    pub holds: u32,
    /// Keep the arena alive even when empty.
    pub keep_alive: bool,
    /// Module-owned per-arena data.
    pub ext: Extensions,
}

impl Arena {
    /// True for public arenas (`"0"`, `"1"`, ...).
    pub fn is_public(&self) -> bool {
        self.basename == PUBLIC_BASENAME
    }
}

/// The arena table.
#[derive(Default)]
pub struct Arenas {
    map: HashMap<ArenaId, Arena>,
    by_name: HashMap<String, ArenaId>,
    next: u32,
}

impl Arenas {
    /// Creates an arena in [`ArenaStatus::DoInit0`] with an empty config; the
    /// arena manager fills in the config and drives its lifecycle.
    pub fn create(&mut self, name: &str) -> ArenaId {
        let id = ArenaId(self.next);
        self.next += 1;
        let arena = Arena {
            id,
            name: name.to_owned(),
            basename: basename(name),
            status: ArenaStatus::DoInit0,
            cfg: Arc::new(ConfigFile::default()),
            attached: Vec::new(),
            holds: 0,
            keep_alive: false,
            ext: Extensions::default(),
        };
        self.by_name.insert(name.to_ascii_lowercase(), id);
        self.map.insert(id, arena);
        id
    }

    /// Removes an arena.
    pub fn remove(&mut self, id: ArenaId) -> Option<Arena> {
        let a = self.map.remove(&id)?;
        if self.by_name.get(&a.name.to_ascii_lowercase()) == Some(&id) {
            self.by_name.remove(&a.name.to_ascii_lowercase());
        }
        Some(a)
    }

    /// Looks up an arena.
    pub fn get(&self, id: ArenaId) -> Option<&Arena> {
        self.map.get(&id)
    }

    /// Looks up an arena mutably.
    pub fn get_mut(&mut self, id: ArenaId) -> Option<&mut Arena> {
        self.map.get_mut(&id)
    }

    /// Finds an arena by name (ASCII case-insensitive).
    pub fn find(&self, name: &str) -> Option<ArenaId> {
        self.by_name.get(&name.to_ascii_lowercase()).copied()
    }

    /// All arenas.
    pub fn iter(&self) -> impl Iterator<Item = &Arena> {
        self.map.values()
    }

    /// Snapshot of arena ids.
    pub fn ids(&self) -> Vec<ArenaId> {
        self.map.keys().copied().collect()
    }

    /// Number of arenas.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when no arenas exist.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basenames_follow_asss() {
        assert_eq!(basename("0"), "(public)");
        assert_eq!(basename("12"), "(public)");
        assert_eq!(basename("turf"), "turf");
        assert_eq!(basename("turf2"), "turf");
        assert_eq!(basename("a1b2"), "a1b");
        assert_eq!(
            basename("abcdefghijklmnopqrstuvwxyz"),
            "abcdefghijklmnopqrs"
        );
    }
}
