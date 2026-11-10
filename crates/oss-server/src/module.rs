//! Modules: units of server functionality loaded at startup (ASSS `modman`).
//!
//! A module is a [`Module`] value created by a factory registered in the
//! [`ModuleCatalog`] under a name (plus ASSS aliases, so an ASSS
//! `conf/modules.conf` can be read unchanged). Loading calls
//! [`Module::load`], where the module registers interfaces, event and packet
//! handlers, commands and timers, all tagged with its [`Owner`] so they can be
//! removed again. Modules that implement [`Module::attach`] can be attached to
//! individual arenas (ASSS `AttachModules`).
//!
//! Private builds add modules by registering more factories before the server
//! starts (see [`crate::runtime::ServerBuilder::module`]); nothing in this
//! crate needs patching.

use std::sync::Arc;

use crate::arena::ArenaId;
use crate::registry::Owner;
use crate::zone::Zone;

/// Errors from module operations.
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    /// No factory with that name.
    #[error("unknown module '{0}'")]
    Unknown(String),
    /// Already loaded.
    #[error("module '{0}' is already loaded")]
    AlreadyLoaded(String),
    /// Not loaded.
    #[error("module '{0}' is not loaded")]
    NotLoaded(String),
    /// The module can't be attached to arenas.
    #[error("module '{0}' can't be attached to arenas")]
    NotAttachable(String),
    /// A module this one needs is missing.
    #[error("module '{module}' needs {needs}")]
    Missing {
        /// Module that failed.
        module: String,
        /// What it needed.
        needs: String,
    },
    /// Anything else.
    #[error("module '{module}' failed: {reason}")]
    Failed {
        /// Module that failed.
        module: String,
        /// Why.
        reason: String,
    },
}

/// A server module. Modules keep their state in zone resources or player/arena
/// extensions rather than in `self`, so a module value is shared (`Arc`) and
/// its methods take `&self`.
pub trait Module: Send + Sync + 'static {
    /// Registers interfaces, handlers, commands and timers.
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError>;

    /// Runs after every module in the initial set has loaded (ASSS
    /// `MM_POSTLOAD`): look up optional interfaces of other modules here.
    fn post_load(&self, _zone: &mut Zone, _me: Owner) {}

    /// Releases resources. Everything registered under `me` is removed by the
    /// manager afterwards, so most modules don't need this.
    fn unload(&self, _zone: &mut Zone, _me: Owner) {}

    /// Attaches the module to an arena: register arena-scoped handlers.
    fn attach(&self, _zone: &mut Zone, _me: Owner, _arena: ArenaId) -> Result<(), ModuleError> {
        Err(ModuleError::NotAttachable(String::new()))
    }

    /// Detaches from an arena. Arena-scoped registrations under `me` are
    /// removed by the manager afterwards.
    fn detach(&self, _zone: &mut Zone, _me: Owner, _arena: ArenaId) {}
}

/// Creates a module instance.
pub type Factory = Arc<dyn Fn() -> Arc<dyn Module> + Send + Sync>;

struct Spec {
    name: String,
    aliases: Vec<String>,
    factory: Factory,
}

/// Names of ASSS 1.4.4 modules whose job the zone core does itself (logging,
/// timers, config, player table, the network layer, ...). Listing them in
/// `modules.conf` is accepted and does nothing.
pub const BUILT_INTO_CORE: &[&str] = &[
    "unixsignal",
    "deadlock",
    "prng",
    "logman",
    "log_console",
    "log_file",
    "mainloop",
    "config",
    "playerdata",
    "bw_default",
    "bw_nolimit",
    "net",
    "core",
];

/// All modules a server can load, by name.
#[derive(Default, Clone)]
pub struct ModuleCatalog {
    specs: Vec<Arc<Spec>>,
}

fn norm(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

impl ModuleCatalog {
    /// Registers a factory under `name` and any `aliases` (e.g. ASSS names
    /// such as `"scoring:persist"`). A later registration with the same name
    /// replaces the earlier one, so private builds can swap implementations.
    pub fn register<M: Module>(
        &mut self,
        name: &str,
        aliases: &[&str],
        factory: impl Fn() -> M + Send + Sync + 'static,
    ) {
        let name = norm(name);
        self.specs.retain(|s| s.name != name);
        self.specs.push(Arc::new(Spec {
            name,
            aliases: aliases.iter().map(|a| norm(a)).collect(),
            factory: Arc::new(move || Arc::new(factory()) as Arc<dyn Module>),
        }));
    }

    fn find(&self, name: &str) -> Option<Arc<Spec>> {
        let n = norm(name);
        self.specs
            .iter()
            .rev()
            .find(|s| s.name == n || s.aliases.contains(&n))
            .cloned()
    }

    /// Canonical name for `name` (resolving aliases).
    pub fn canonical(&self, name: &str) -> Option<String> {
        self.find(name).map(|s| s.name.clone())
    }

    /// Registered canonical names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.specs.iter().map(|s| s.name.as_str())
    }

    /// The other names `name` answers to (its ASSS names), if it is
    /// registered.
    pub fn aliases(&self, name: &str) -> Vec<String> {
        self.find(name)
            .map(|s| s.aliases.clone())
            .unwrap_or_default()
    }
}

struct Loaded {
    owner: Owner,
    name: String,
    module: Arc<dyn Module>,
}

/// The loaded modules of a zone.
#[derive(Default)]
pub struct ModuleManager {
    catalog: ModuleCatalog,
    loaded: Vec<Loaded>,
    next_owner: u32,
}

impl ModuleManager {
    /// A manager that can load modules from `catalog`.
    pub fn new(catalog: ModuleCatalog) -> Self {
        Self {
            catalog,
            loaded: Vec::new(),
            next_owner: 1,
        }
    }

    /// The catalog.
    pub fn catalog(&self) -> &ModuleCatalog {
        &self.catalog
    }

    /// Canonical names of loaded modules, in load order.
    pub fn loaded(&self) -> impl Iterator<Item = &str> {
        self.loaded.iter().map(|l| l.name.as_str())
    }

    /// True if a module with this name (or alias) is loaded.
    pub fn is_loaded(&self, name: &str) -> bool {
        let n = self.catalog.canonical(name).unwrap_or_else(|| norm(name));
        self.loaded.iter().any(|l| l.name == n)
    }

    /// The owner tag of a loaded module.
    pub fn owner_of(&self, name: &str) -> Option<Owner> {
        let n = self.catalog.canonical(name).unwrap_or_else(|| norm(name));
        self.loaded.iter().find(|l| l.name == n).map(|l| l.owner)
    }

    fn by_owner(&self, owner: Owner) -> Option<(String, Arc<dyn Module>)> {
        self.loaded
            .iter()
            .find(|l| l.owner == owner)
            .map(|l| (l.name.clone(), l.module.clone()))
    }
}

/// Loads a module by name or alias. ASSS core module names in
/// [`BUILT_INTO_CORE`] succeed without doing anything.
pub fn load(zone: &mut Zone, name: &str) -> Result<Option<Owner>, ModuleError> {
    let Some(spec) = zone.modules.catalog.find(name) else {
        if BUILT_INTO_CORE.contains(&norm(name).as_str()) {
            return Ok(None);
        }
        return Err(ModuleError::Unknown(name.to_owned()));
    };
    if zone.modules.loaded.iter().any(|l| l.name == spec.name) {
        return Err(ModuleError::AlreadyLoaded(spec.name.clone()));
    }
    let owner = Owner(zone.modules.next_owner);
    zone.modules.next_owner += 1;
    let module = (spec.factory)();
    if let Err(e) = module.load(zone, owner) {
        cleanup(zone, owner);
        return Err(e);
    }
    tracing::debug!(target: "modman", module = %spec.name, "loaded");
    zone.modules.loaded.push(Loaded {
        owner,
        name: spec.name.clone(),
        module,
    });
    Ok(Some(owner))
}

/// Loads one module at run time (ASSS `?insmod`): [`load`] followed by its
/// [`Module::post_load`].
pub fn insert(zone: &mut Zone, name: &str) -> Result<Option<Owner>, ModuleError> {
    let owner = load(zone, name)?;
    if let Some(o) = owner
        && let Some((_, m)) = zone.modules.by_owner(o)
    {
        m.post_load(zone, o);
    }
    Ok(owner)
}

/// Calls [`Module::post_load`] on every loaded module, in load order.
pub fn post_load_all(zone: &mut Zone) {
    let all: Vec<(Owner, Arc<dyn Module>)> = zone
        .modules
        .loaded
        .iter()
        .map(|l| (l.owner, l.module.clone()))
        .collect();
    for (owner, m) in all {
        m.post_load(zone, owner);
    }
}

fn cleanup(zone: &mut Zone, owner: Owner) {
    zone.reg.remove_owner(owner);
    zone.timers.cancel_owner(owner);
    zone.commands.remove_owner(owner);
}

/// Unloads one module.
pub fn unload(zone: &mut Zone, name: &str) -> Result<(), ModuleError> {
    let owner = zone
        .modules
        .owner_of(name)
        .ok_or_else(|| ModuleError::NotLoaded(name.to_owned()))?;
    unload_owner(zone, owner);
    Ok(())
}

fn unload_owner(zone: &mut Zone, owner: Owner) {
    let Some((name, m)) = zone.modules.by_owner(owner) else {
        return;
    };
    for a in zone.arenas.ids() {
        if zone
            .arenas
            .get(a)
            .is_some_and(|x| x.attached.contains(&owner))
        {
            detach_owner(zone, owner, a);
        }
    }
    m.unload(zone, owner);
    cleanup(zone, owner);
    zone.modules.loaded.retain(|l| l.owner != owner);
    tracing::debug!(target: "modman", module = %name, "unloaded");
}

/// Unloads every module, newest first.
pub fn unload_all(zone: &mut Zone) {
    while let Some(owner) = zone.modules.loaded.last().map(|l| l.owner) {
        unload_owner(zone, owner);
    }
}

/// Attaches a loaded module to an arena.
pub fn attach(zone: &mut Zone, name: &str, arena: ArenaId) -> Result<(), ModuleError> {
    let owner = zone
        .modules
        .owner_of(name)
        .ok_or_else(|| ModuleError::NotLoaded(name.to_owned()))?;
    let Some((canon, m)) = zone.modules.by_owner(owner) else {
        return Err(ModuleError::NotLoaded(name.to_owned()));
    };
    if zone
        .arenas
        .get(arena)
        .is_some_and(|a| a.attached.contains(&owner))
    {
        return Ok(());
    }
    match m.attach(zone, owner, arena) {
        Ok(()) => {
            if let Some(a) = zone.arenas.get_mut(arena) {
                a.attached.push(owner);
            }
            Ok(())
        }
        Err(ModuleError::NotAttachable(_)) => Err(ModuleError::NotAttachable(canon)),
        Err(e) => {
            zone.reg.remove_owner_in_arena(owner, arena);
            Err(e)
        }
    }
}

fn detach_owner(zone: &mut Zone, owner: Owner, arena: ArenaId) {
    if let Some((_, m)) = zone.modules.by_owner(owner) {
        m.detach(zone, owner, arena);
    }
    zone.reg.remove_owner_in_arena(owner, arena);
    zone.commands.remove_owner_in_arena(owner, arena);
    if let Some(a) = zone.arenas.get_mut(arena) {
        a.attached.retain(|o| *o != owner);
    }
}

/// Detaches a module from an arena.
pub fn detach(zone: &mut Zone, name: &str, arena: ArenaId) -> Result<(), ModuleError> {
    let owner = zone
        .modules
        .owner_of(name)
        .ok_or_else(|| ModuleError::NotLoaded(name.to_owned()))?;
    detach_owner(zone, owner, arena);
    Ok(())
}

/// Detaches every module from an arena (newest first), e.g. before it is destroyed.
pub fn detach_all(zone: &mut Zone, arena: ArenaId) {
    while let Some(owner) = zone
        .arenas
        .get(arena)
        .and_then(|a| a.attached.last().copied())
    {
        detach_owner(zone, owner, arena);
    }
}

/// Reads an ASSS `conf/modules.conf` (through the config preprocessor, as
/// ASSS does): one module name per content line.
pub fn read_modules_conf(src: &dyn crate::config::ConfSource) -> Option<Vec<String>> {
    let mut pp = crate::config::Preprocessor::new(src, None);
    if !pp.add_file("modules.conf") {
        return None;
    }
    let mut names = Vec::new();
    while let Some(line) = pp.next_line() {
        let name = crate::config::latin1(&line).trim().to_owned();
        if !name.is_empty() {
            names.push(name);
        }
    }
    for e in pp.take_errors() {
        tracing::warn!(target: "modman", "modules.conf: {e}");
    }
    Some(names)
}
