//! `cmdman`: runs `?commands` with capability checks (ASSS `cmdman.c`).
//!
//! A command typed in public needs the capability `cmd_<name>`; one sent
//! privately to a player or a frequency needs `privcmd_<name>`; one sent to a
//! player in another arena needs `rprivcmd_<name>` there. Without a capability
//! manager every command is allowed (ASSS `CFG_ALLOW_ALL_IF_CAPMAN_IS_MISSING`).

use crate::arena::ArenaId;
use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::Owner;
use crate::zone::Zone;

use super::capman::Capabilities;

/// Longest command name ASSS accepts.
const MAX_NAME: usize = 30;

fn allowed(
    zone: &mut Zone,
    player: PlayerId,
    cmd: &str,
    prefix: &str,
    remote: Option<ArenaId>,
) -> bool {
    let Some(cm) = zone.iface::<dyn Capabilities>() else {
        return true;
    };
    let cap = format!("{prefix}_{cmd}");
    match remote {
        Some(a) => cm.has_capability_in_arena(zone, player, a, &cap),
        None => cm.has_capability(zone, player, &cap),
    }
}

/// Splits `line` into a lowercased command name and its parameters (ASSS:
/// the name ends at a space or `=`, spaces and `=` before the parameters are
/// skipped).
pub fn split_command(line: &str) -> (String, &str) {
    let end = line
        .char_indices()
        .take(MAX_NAME)
        .find(|&(_, c)| c == ' ' || c == '=')
        .map_or_else(
            || {
                line.char_indices()
                    .nth(MAX_NAME)
                    .map_or(line.len(), |(i, _)| i)
            },
            |(i, _)| i,
        );
    let name = line[..end].to_ascii_lowercase();
    let params = line[end..].trim_start_matches([' ', '=']);
    (name, params)
}

/// ASSS `Command`: runs one command line (without the leading `?`).
pub fn command(zone: &mut Zone, sender: PlayerId, line: &str, target: Target) {
    let Some(arena) = zone.players.get(sender).and_then(|p| p.arena) else {
        return;
    };
    // a leading backslash sends the command to the billing server only
    let (line, skip_local) = match line.strip_prefix('\\') {
        Some(l) => (l, true),
        None => (line, false),
    };
    let (name, params) = split_command(line);
    if name.is_empty() {
        return;
    }
    let (prefix, remote) = match target {
        Target::Arena(_) | Target::None => ("cmd", None),
        Target::Player(t) => match zone.players.get(t).and_then(|p| p.arena) {
            Some(ta) if ta == arena => ("privcmd", None),
            other => ("rprivcmd", other),
        },
        Target::Freq(..) => ("privcmd", None),
    };
    let handlers = if skip_local {
        Vec::new()
    } else {
        zone.commands.lookup(&name, Some(arena))
    };
    if handlers.is_empty() {
        // we don't know about this: send it to the biller
        match zone.commands.default_handler() {
            Some(f) => f(
                zone,
                &Invocation {
                    name: &name,
                    params: line,
                    sender,
                    arena: Some(arena),
                    target,
                },
            ),
            None => {
                tracing::debug!(target: "cmdman", pid = sender.0, cmd = %name, "unknown command");
            }
        }
        return;
    }
    if !allowed(zone, sender, &name, prefix, remote) {
        tracing::debug!(target: "cmdman", pid = sender.0, cmd = %name, "permission denied");
        return;
    }
    let inv = Invocation {
        name: &name,
        params,
        sender,
        arena: Some(arena),
        target,
    };
    for c in handlers {
        (c.handler)(zone, &inv);
    }
    tracing::info!(target: "cmdman", pid = sender.0, cmd = %name, params, "command");
}

/// The `cmdman` module. The command table lives in the zone; this module
/// only provides the dispatcher, so it has nothing to register.
pub struct CmdManModule;

impl Module for CmdManModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_asss() {
        assert_eq!(split_command("GO turf"), ("go".to_owned(), "turf"));
        assert_eq!(split_command("set=  =5"), ("set".to_owned(), "5"));
        assert_eq!(split_command("help"), ("help".to_owned(), ""));
        let long = "a".repeat(40);
        assert_eq!(split_command(&long).0.len(), 30);
    }
}
