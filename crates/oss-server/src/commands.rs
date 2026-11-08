//! The command table (`?command` and `*command`), ASSS `cmdman`'s registry.
//!
//! Modules register handlers by name, globally or for an arena they're
//! attached to. Parsing chat into commands, targets and capability checks are
//! done by the `cmdman` module; this is only the table.

use std::collections::HashMap;
use std::sync::Arc;

use crate::arena::ArenaId;
use crate::player::PlayerId;
use crate::registry::Owner;
use crate::zone::Zone;

/// Who a command was aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Typed in public chat: the sender's arena.
    Arena(ArenaId),
    /// Typed in a team message to a frequency.
    Freq(ArenaId, u16),
    /// Typed in a private message to a player.
    Player(PlayerId),
    /// No target (e.g. from the console or a remote admin).
    None,
}

/// One command invocation.
#[derive(Debug, Clone)]
pub struct Invocation<'a> {
    /// Command name, lowercased.
    pub name: &'a str,
    /// Everything after the name (leading spaces removed).
    pub params: &'a str,
    /// Who typed it.
    pub sender: PlayerId,
    /// Arena the sender is in.
    pub arena: Option<ArenaId>,
    /// What it was aimed at.
    pub target: Target,
}

/// A command handler.
pub type CommandFn = Arc<dyn Fn(&mut Zone, &Invocation<'_>) + Send + Sync>;

/// A registered command.
#[derive(Clone)]
pub struct Command {
    /// Registering module.
    pub owner: Owner,
    /// Arena it is limited to, if any.
    pub arena: Option<ArenaId>,
    /// The handler.
    pub handler: CommandFn,
    /// Help text for `?help`.
    pub help: Option<Arc<str>>,
}

/// All registered commands.
#[derive(Default)]
pub struct Commands {
    map: HashMap<String, Vec<Command>>,
    default: Option<(Owner, CommandFn)>,
}

impl Commands {
    /// Registers `name` (case-insensitive).
    pub fn add(
        &mut self,
        owner: Owner,
        name: &str,
        arena: Option<ArenaId>,
        help: Option<&str>,
        handler: CommandFn,
    ) {
        self.map
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(Command {
                owner,
                arena,
                handler,
                help: help.map(Arc::from),
            });
    }

    /// Handlers that apply to `name` for a player in `arena`: global ones and
    /// those registered for that arena.
    pub fn lookup(&self, name: &str, arena: Option<ArenaId>) -> Vec<Command> {
        self.map
            .get(&name.to_ascii_lowercase())
            .map(|v| {
                v.iter()
                    .filter(|c| c.arena.is_none() || c.arena == arena)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Help text for `name`, if any handler provided one.
    pub fn help(&self, name: &str) -> Option<Arc<str>> {
        self.map
            .get(&name.to_ascii_lowercase())?
            .iter()
            .find_map(|c| c.help.clone())
    }

    /// Sets the handler for commands nobody registered and for lines starting
    /// with a backslash (ASSS `AddCommand(NULL, ...)`; the billing module
    /// forwards these to the biller). Its [`Invocation::params`] is the whole
    /// line without the `?` or backslash.
    pub fn set_default(&mut self, owner: Owner, handler: CommandFn) {
        self.default = Some((owner, handler));
    }

    /// The default handler, if one is set.
    pub fn default_handler(&self) -> Option<CommandFn> {
        self.default.as_ref().map(|(_, f)| f.clone())
    }

    /// Registered command names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    /// Removes everything `owner` registered.
    pub fn remove_owner(&mut self, owner: Owner) {
        if self.default.as_ref().is_some_and(|(o, _)| *o == owner) {
            self.default = None;
        }
        for v in self.map.values_mut() {
            v.retain(|c| c.owner != owner);
        }
        self.map.retain(|_, v| !v.is_empty());
    }

    /// Removes what `owner` registered for `arena`.
    pub fn remove_owner_in_arena(&mut self, owner: Owner, arena: ArenaId) {
        for v in self.map.values_mut() {
            v.retain(|c| !(c.owner == owner && c.arena == Some(arena)));
        }
        self.map.retain(|_, v| !v.is_empty());
    }
}
