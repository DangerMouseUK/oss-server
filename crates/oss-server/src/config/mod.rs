//! ASSS-compatible configuration.
//!
//! Zone directories are read unchanged: `conf/global.conf`,
//! `arenas/<name>/arena.conf`, `#include` trees, `groupdef.conf`, ... The
//! parsing rules (preprocessor directives, sections, escapes, case-insensitive
//! keys, "last definition wins", `strtol`-style integers) follow ASSS 1.4.4's
//! `app.c` and `config.c`; see [`app`] and [`file`](mod@file) for the details and
//! the few deliberate divergences.
//!
//! Text is decoded as Latin-1, so every byte of a config value survives and can
//! be sent to clients unchanged with [`to_latin1`].

pub mod app;
pub mod file;
pub mod search;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub use app::{ConfSource, Preprocessor};
pub use file::{ConfigFile, config_int, strtol32};

/// Decodes bytes as Latin-1 (every byte maps to the code point of the same value).
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Encodes a string as Latin-1; characters above U+00FF become `?`.
pub fn to_latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// A zone directory on disk (the directory holding `conf/`, `arenas/`, `maps/`).
#[derive(Debug, Clone)]
pub struct ZoneDir {
    root: PathBuf,
}

impl ZoneDir {
    /// Wraps a zone root directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The zone root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Finds a file on an arbitrary ASSS search path relative to the zone root.
    pub fn find(&self, search_path: &str, repls: &[(char, &str)]) -> Option<PathBuf> {
        search::find_file_on_path(&self.root, search_path, repls)
    }
}

impl ConfSource for ZoneDir {
    fn find(&self, search_path: &str, repls: &[(char, &str)]) -> Option<PathBuf> {
        search::find_file_on_path(&self.root, search_path, repls)
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(path)).ok()
    }

    fn modified(&self, path: &Path) -> Option<SystemTime> {
        std::fs::metadata(self.root.join(path))
            .and_then(|m| m.modified())
            .ok()
    }

    fn append(&self, path: &Path, text: &[u8]) -> std::io::Result<bool> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.root.join(path))?;
        f.write_all(text)?;
        Ok(true)
    }
}

/// In-memory files keyed by zone-relative path; resolves exactly like
/// [`ZoneDir`]. Intended for tests and embedded configurations.
#[derive(Debug, Clone, Default)]
pub struct MemSource {
    files: HashMap<String, Vec<u8>>,
}

impl MemSource {
    /// Builds a source from `(relative path, contents)` pairs.
    pub fn new<P: Into<String>, D: AsRef<[u8]>>(files: impl IntoIterator<Item = (P, D)>) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|(p, d)| (p.into(), d.as_ref().to_vec()))
                .collect(),
        }
    }

    /// Adds or replaces a file.
    pub fn insert(&mut self, path: impl Into<String>, data: impl AsRef<[u8]>) {
        self.files.insert(path.into(), data.as_ref().to_vec());
    }
}

impl ConfSource for MemSource {
    fn find(&self, search_path: &str, repls: &[(char, &str)]) -> Option<PathBuf> {
        search::find_on_path(search_path, repls, |p| self.files.contains_key(p)).map(PathBuf::from)
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let key = path.to_string_lossy().replace('\\', "/");
        self.files.get(&key).cloned()
    }
}

struct Cached {
    file: Arc<ConfigFile>,
    arena: Option<String>,
    name: String,
    stamps: Vec<Option<SystemTime>>,
}

fn stamps(src: &dyn ConfSource, files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    files.iter().map(|p| src.modified(p)).collect()
}

/// Opens and caches config files for the zone. Files are shared by resolved
/// path, as in ASSS (two arenas whose `arena.conf` resolves to the same file
/// share one parse).
///
/// Loaded files are immutable [`Arc`]s; a reload builds a new one and the
/// caller distributes it (see `ConfigChanged` events in the zone core).
pub struct ConfigManager {
    source: Box<dyn ConfSource + Send + Sync>,
    cache: HashMap<PathBuf, Cached>,
    /// (arena, lowercased name) -> resolved path, so repeated opens don't touch disk
    resolved: HashMap<(Option<String>, String), PathBuf>,
    global: Arc<ConfigFile>,
}

/// The base file a config open asked for could not be found.
#[derive(Debug, thiserror::Error)]
#[error("config file not found: arena {arena:?}, name {name:?}")]
pub struct NotFound {
    /// Arena the lookup was for.
    pub arena: Option<String>,
    /// File name looked up.
    pub name: String,
}

impl ConfigManager {
    /// Opens the zone's `global.conf` from `source`.
    pub fn new(source: impl ConfSource + Send + Sync + 'static) -> Result<Self, NotFound> {
        let mut m = Self {
            source: Box::new(source),
            cache: HashMap::new(),
            resolved: HashMap::new(),
            global: Arc::new(ConfigFile::default()),
        };
        m.global = m.open(None, "global.conf")?;
        Ok(m)
    }

    /// Where zone files come from (config, maps, news).
    pub fn source(&self) -> &dyn ConfSource {
        self.source.as_ref()
    }

    /// The zone-wide `global.conf`.
    pub fn global(&self) -> &Arc<ConfigFile> {
        &self.global
    }

    /// Opens `name` for `arena` (`None` = global search path), sharing an
    /// already-loaded parse of the same resolved file.
    pub fn open(&mut self, arena: Option<&str>, name: &str) -> Result<Arc<ConfigFile>, NotFound> {
        let not_found = || NotFound {
            arena: arena.map(str::to_owned),
            name: name.to_owned(),
        };
        let req = (arena.map(str::to_owned), name.to_ascii_lowercase());
        if let Some(c) = self.resolved.get(&req).and_then(|p| self.cache.get(p)) {
            return Ok(c.file.clone());
        }
        // resolve first so that the cache is keyed by the actual file
        let (path, _) = self.source.open(arena, name).ok_or_else(not_found)?;
        self.resolved.insert(req, path.clone());
        if let Some(c) = self.cache.get(&path) {
            return Ok(c.file.clone());
        }
        let file = ConfigFile::load(self.source.as_ref(), arena, name).ok_or_else(not_found)?;
        for w in file.warnings() {
            tracing::warn!(target: "config", file = %path.display(), "{w}");
        }
        let file = Arc::new(file);
        self.cache.insert(
            path,
            Cached {
                stamps: stamps(self.source.as_ref(), file.sources()),
                file: file.clone(),
                arena: arena.map(str::to_owned),
                name: name.to_owned(),
            },
        );
        Ok(file)
    }

    /// Opens an arena's `arena.conf` (looked up for the arena's base name).
    pub fn open_arena(&mut self, arena_base: &str) -> Result<Arc<ConfigFile>, NotFound> {
        self.open(Some(arena_base), "arena.conf")
    }

    /// Re-reads files whose base file or any include changed on disk. Returns
    /// the reloaded files as `(old, new)` pairs so callers can swap handles.
    pub fn reload_changed(&mut self) -> Vec<(Arc<ConfigFile>, Arc<ConfigFile>)> {
        let mut changed = Vec::new();
        let paths: Vec<PathBuf> = self.cache.keys().cloned().collect();
        for path in paths {
            let Some(c) = self.cache.get(&path) else {
                continue;
            };
            if stamps(self.source.as_ref(), c.file.sources()) == c.stamps {
                continue;
            }
            let Some(new) = ConfigFile::load(self.source.as_ref(), c.arena.as_deref(), &c.name)
            else {
                continue;
            };
            let new = Arc::new(new);
            let old = c.file.clone();
            if Arc::ptr_eq(&old, &self.global) {
                self.global = new.clone();
            }
            self.cache.insert(
                path,
                Cached {
                    stamps: stamps(self.source.as_ref(), new.sources()),
                    file: new.clone(),
                    arena: c.arena.clone(),
                    name: c.name.clone(),
                },
            );
            changed.push((old, new));
        }
        changed
    }

    /// ASSS `SetStr`: changes `section:key` in the loaded file `file` (as
    /// returned by [`ConfigManager::open`]) and returns the updated file. With
    /// `permanent`, the change is also appended to the base file on disk as
    /// `; info` + `Section:Key = value`, the way ASSS writes dirty values.
    pub fn set(
        &mut self,
        file: &Arc<ConfigFile>,
        section: &str,
        key: &str,
        value: &str,
        info: Option<&str>,
        permanent: bool,
    ) -> Arc<ConfigFile> {
        let path = file.path().to_path_buf();
        let Some(c) = self.cache.get_mut(&path) else {
            return file.clone();
        };
        if c.file.get(section, key) == Some(value) {
            return c.file.clone();
        }
        let mut updated = (*c.file).clone();
        updated.set(section, key, value);
        let updated = Arc::new(updated);
        if Arc::ptr_eq(&c.file, &self.global) {
            self.global = updated.clone();
        }
        c.file = updated.clone();
        if permanent {
            let esc = |s: &str| s.replace('\\', "\\\\").replace('=', "\\=");
            let mut text = String::new();
            if let Some(i) = info {
                text.push_str(&format!("; {i}\n"));
            }
            text.push_str(&format!(
                "{} = {}\n\n",
                esc(&format!("{section}:{key}")),
                esc(value)
            ));
            match self.source.append(&path, &to_latin1(&text)) {
                Ok(_) => c.stamps = stamps(self.source.as_ref(), updated.sources()),
                Err(e) => {
                    tracing::warn!(target: "config", file = %path.display(), "failed to write setting: {e}");
                }
            }
        }
        updated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_shares_by_resolved_path() {
        let src = MemSource::new([
            ("conf/global.conf", "[General]\nName = x\n"),
            ("arenas/(default)/arena.conf", "[Misc]\nA = 1\n"),
            (
                "arenas/turf/arena.conf",
                "#include arenas/(default)/arena.conf\n[Misc]\nA = 2\n",
            ),
        ]);
        let mut m = ConfigManager::new(src).expect("global");
        assert_eq!(m.global().get("General", "Name"), Some("x"));
        let turf = m.open_arena("turf").expect("turf");
        let pb = m.open_arena("pb").expect("pb falls back to (default)");
        let other = m.open_arena("other").expect("other");
        assert_eq!(turf.get_int("Misc", "A", 0), 2);
        assert_eq!(pb.get_int("Misc", "A", 0), 1);
        assert!(Arc::ptr_eq(&pb, &other));
    }

    #[test]
    fn missing_global_is_an_error() {
        assert!(ConfigManager::new(MemSource::default()).is_err());
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[test]
    fn set_writes_back_and_rereads_identically() {
        let dir = std::env::temp_dir().join(format!("oss-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conf")).expect("mkdir");
        std::fs::write(dir.join("conf/global.conf"), "").expect("global");
        std::fs::write(dir.join("conf/passwd.conf"), "[Users]\nold = any\n").expect("passwd");
        let mut m = ConfigManager::new(ZoneDir::new(&dir)).expect("mgr");
        let f = m.open(None, "passwd.conf").expect("passwd");
        let name = r"we=ird\name";
        let f2 = m.set(&f, "Users", name, "hash", Some("set by test"), true);
        assert_eq!(f2.get("users", name), Some("hash"));
        assert!(Arc::ptr_eq(
            &m.open(None, "passwd.conf").expect("again"),
            &f2
        ));
        assert!(m.reload_changed().is_empty());
        let reread = ConfigFile::load(&ZoneDir::new(&dir), None, "passwd.conf").expect("reread");
        assert_eq!(reread.get("Users", name), Some("hash"));
        assert_eq!(reread.get("Users", "old"), Some("any"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
