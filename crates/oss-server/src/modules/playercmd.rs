//! `playercmd`: everyday player commands (a first slice of ASSS
//! `playercmd.c`): `?go`, `?help`, `?version`.

use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::player::Ship;
use crate::registry::Owner;
use crate::zone::Zone;

use super::arenaman::{GoRequest, go};
use super::chat::message;

fn c_go(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(p) = zone.players.get(inv.sender) else {
        return;
    };
    let name = if inv.params.is_empty() {
        "0".to_owned()
    } else {
        inv.params.to_owned()
    };
    let req = GoRequest {
        name,
        ship: Ship::SPEC,
        resolution: p.resolution,
        want_all_lvz: p.want_all_lvz,
        accept_audio: p.accept_audio,
        obscenity_filter: p.obscenity_filter,
    };
    go(zone, inv.sender, req);
}

fn c_help(zone: &mut Zone, inv: &Invocation<'_>) {
    let topic = inv.params.trim_start_matches(['?', '*']);
    let topic = if topic.is_empty() { "help" } else { topic };
    match zone.commands.help(topic) {
        Some(text) => {
            message(zone, inv.sender, &format!("Help on '?{topic}':"));
            for line in text.lines() {
                message(zone, inv.sender, &format!("  {line}"));
            }
        }
        None => message(
            zone,
            inv.sender,
            &format!("Sorry, I don't know anything about ?{topic}"),
        ),
    }
}

/// ASSS `?sheep`: `Misc:SheepMessage`, with sound 24, to the sender.
fn c_sheep(zone: &mut Zone, inv: &Invocation<'_>) {
    let crate::commands::Target::Arena(a) = inv.target else {
        return;
    };
    let text = zone
        .arenas
        .get(a)
        .and_then(|x| x.cfg.get("Misc", "SheepMessage").map(str::to_owned))
        .unwrap_or_else(|| "Sheep successfully cloned -- hello Dolly".to_owned());
    if let Some(c) = zone.iface::<dyn super::chat::Chat>() {
        c.send_any_message(
            zone,
            &[inv.sender],
            oss_proto::common::chat_type::ARENA,
            24,
            -1,
            &text,
        );
    }
}

fn c_version(zone: &mut Zone, inv: &Invocation<'_>) {
    message(
        zone,
        inv.sender,
        &format!("Open SubSpace server {}", env!("CARGO_PKG_VERSION")),
    );
}

/// The `playercmd` module.
pub struct PlayerCmdModule;

impl Module for PlayerCmdModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.add_command(
            me,
            "go",
            None,
            Some("Module: playercmd\nTargets: none\nArgs: [<arena name>]\nMoves you to the named arena, or a public one.\n"),
            c_go,
        );
        zone.add_command(
            me,
            "help",
            None,
            Some("Module: playercmd\nTargets: none\nArgs: <command name>\nShows help on a command.\n"),
            c_help,
        );
        zone.add_command(
            me,
            "version",
            None,
            Some("Module: playercmd\nTargets: none\nArgs: none\nShows the server version.\n"),
            c_version,
        );
        zone.add_command(me, "sheep", None, None, c_sheep);
        Ok(())
    }
}
