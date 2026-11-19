//! `notify` (ASSS `core/notify.c`): `?cheater`, a message from any player to
//! every member of staff who is online.
//!
//! The command names come from the global `Notify:AlertCommand`, a list split
//! on spaces, commas, colons and semicolons (default `cheater`). A player who
//! may use staff chat's mask (who has not been silenced from it) can send
//! one; the zone's chat module tells players without the staff-chat
//! capability to use it.
//!
//! Two differences from ASSS, both taken from Subspace Server .NET's `Notify`
//! module (`Notify.cs`), which describes them:
//!
//! * A command with no message is refused with `Notify:EmptyReply` rather
//!   than sending staff an empty alert.
//! * The alert reads as subgame's does, `cheater: (name) (arena): message`,
//!   with `Public ` before a public arena's name, rather than ASSS's
//!   `cheater {arena} name: message`.

use oss_proto::common::chat_type;

use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::Owner;
use crate::zone::Zone;

use super::capman::{CAP_MOD_CHAT, Capabilities};
use super::chat::{Chat, ChatImpl, allowed, mask_type, message};

/// The command names used when `Notify:AlertCommand` is unset.
pub const DEFAULT_ALERT_COMMAND: &str = "cheater";

/// The reply to a command with no message, when `Notify:EmptyReply` is unset.
pub const DEFAULT_EMPTY_REPLY: &str = "Please include a message to send to online staff.";

const HELP: &str = "Module: notify\nTargets: none\nArgs: <message>\n\
                    Sends the message to all online staff members.\n";

/// The command names `Notify:AlertCommand` lists, lowercased.
pub fn alert_commands(setting: Option<&str>) -> Vec<String> {
    let list = setting
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_ALERT_COMMAND);
    let mut names: Vec<String> = Vec::new();
    for word in list.split([' ', ',', ':', ';']).filter(|w| !w.is_empty()) {
        let word = word.to_ascii_lowercase();
        if !names.contains(&word) {
            names.push(word);
        }
    }
    names
}

/// The alert staff see.
pub fn alert_text(command: &str, name: &str, arena: &str, public: bool, text: &str) -> String {
    let public = if public { "Public " } else { "" };
    format!("{command}: ({name}) ({public}{arena}): {text}")
}

fn c_notify(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    let Some((arena_name, public)) = inv
        .arena
        .and_then(|a| zone.arenas.get(a))
        .map(|a| (a.name.clone(), a.is_public()))
    else {
        return;
    };
    if !allowed(zone, p, mask_type::MODCHAT) {
        return;
    }
    let text = inv.params.trim();
    if text.is_empty() {
        let reply = zone
            .cfg
            .global()
            .get("Notify", "EmptyReply")
            .filter(|r| !r.trim().is_empty())
            .unwrap_or(DEFAULT_EMPTY_REPLY)
            .to_owned();
        message(zone, p, &reply);
        return;
    }
    let name = zone
        .players
        .get(p)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    let alert = alert_text(inv.name, &name, &arena_name, public, text);
    // ASSS SendModMessage: red sysop text to everyone playing who sees staff chat
    let staff: Vec<PlayerId> = match zone.iface::<dyn Capabilities>() {
        Some(caps) => {
            let playing: Vec<PlayerId> = zone
                .players
                .iter()
                .filter(|x| x.status == PlayerStatus::Playing)
                .map(|x| x.id)
                .collect();
            playing
                .into_iter()
                .filter(|&s| caps.has_capability(zone, s, CAP_MOD_CHAT))
                .collect()
        }
        None => Vec::new(),
    };
    ChatImpl.send_any_message(zone, &staff, chat_type::SERVER_ERROR, 0, -1, &alert);
    message(zone, p, "Message has been sent to online staff.");
}

/// The `notify` module.
pub struct NotifyModule;

impl Module for NotifyModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let setting = zone
            .cfg
            .global()
            .get("Notify", "AlertCommand")
            .map(str::to_owned);
        for name in alert_commands(setting.as_deref()) {
            zone.add_command(me, &name, None, Some(HELP), c_notify);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_commands_split_like_asss() {
        assert_eq!(alert_commands(None), vec!["cheater"]);
        assert_eq!(alert_commands(Some("  ")), vec!["cheater"]);
        assert_eq!(
            alert_commands(Some("Cheater, help;staff:sos  cheater")),
            vec!["cheater", "help", "staff", "sos"]
        );
    }

    #[test]
    fn the_alert_reads_as_subgame_writes_it() {
        assert_eq!(
            alert_text("cheater", "Bob", "0", true, "warping"),
            "cheater: (Bob) (Public 0): warping"
        );
        assert_eq!(
            alert_text("help", "Bob", "duel", false, "stuck"),
            "help: (Bob) (duel): stuck"
        );
    }
}
