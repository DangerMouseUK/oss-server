//! `auth_file`: local passwords from `conf/passwd.conf` (ASSS `auth_file.c`).
//!
//! ```text
//! [General]
//! AllowUnknown = yes
//! [Users]
//! somebody = 561fb382be64fdfd6c4bc4450577b966   ; md5 hash, see below
//! bad-person = lock
//! someone = any
//! ```
//!
//! The hash is the lowercase hex MD5 of a 56-byte buffer: the lowercased name
//! (at most 23 bytes) at offset 0 and the password (at most 31 bytes) at
//! offset 24, zero-padded. Hashes written by ASSS work unchanged. Only a
//! correct password marks the player as authenticated (which enables staff
//! groups by name).

use md5::{Digest, Md5};

use crate::commands::{Invocation, Target};
use crate::config::to_latin1;
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::chat::message;
use super::core::{Auth, AuthData, LoginRequest, NewPlayer, auth_code, auth_done};

/// ASSS's password hash, as stored in `passwd.conf`.
pub fn hash_password(name: &str, password: &str) -> String {
    let mut msg = [0u8; 56];
    let n = to_latin1(&name.to_ascii_lowercase());
    let n = &n[..n.len().min(23)];
    msg[..n.len()].copy_from_slice(n);
    let p = to_latin1(password);
    let p = &p[..p.len().min(31)];
    msg[24..24 + p.len()].copy_from_slice(p);
    Md5::digest(msg)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Default)]
struct PwData {
    hash: Option<String>,
}

fn passwd(zone: &mut Zone) -> Option<std::sync::Arc<crate::config::ConfigFile>> {
    zone.cfg.open(None, "passwd.conf").ok()
}

/// The auth_file authenticator.
pub struct AuthFile;

impl Auth for AuthFile {
    fn authenticate(&self, zone: &mut Zone, player: PlayerId, login: &LoginRequest) {
        let name: String = login.name.chars().take(31).collect();
        // anything after '*' is a zone password, not the player's
        let pass = login
            .password
            .split('*')
            .next()
            .unwrap_or_default()
            .to_owned();
        let hash = hash_password(&name, &pass);
        if let Some(p) = zone.players.get_mut(player) {
            p.ext.get_or_default::<PwData>().hash = Some(hash.clone());
        }
        let mut ad = AuthData::ok(&name);
        let file = passwd(zone);
        match file.as_ref().and_then(|f| f.get("users", &name)) {
            Some("lock") => ad.code = auth_code::NO_PERMISSION,
            Some("any") => ad.code = auth_code::OK,
            Some(stored) => {
                if stored == hash {
                    ad.authenticated = true;
                } else {
                    ad.code = auth_code::BAD_PASSWORD;
                }
            }
            None => {
                let allow = file
                    .as_ref()
                    .map_or(1, |f| f.get_int("General", "AllowUnknown", 1));
                if allow == 0 {
                    ad.code = auth_code::NO_PERMISSION2;
                }
            }
        }
        auth_done(zone, player, ad);
    }
}

fn c_passwd(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    if inv.params.is_empty() {
        message(zone, p, "You must specify a password.");
        return;
    }
    let Some(file) = passwd(zone) else { return };
    let (name, authed) = zone
        .players
        .get(p)
        .map(|x| (x.name.clone(), x.authenticated))
        .unwrap_or_default();
    if file.get_int("General", "RequireAuthenticationToSetPassword", 1) != 0 && !authed {
        message(
            zone,
            p,
            "You must be authenticated to change your local password.",
        );
        return;
    }
    let hash = hash_password(&name, inv.params);
    zone.cfg.set(&file, "users", &name, &hash, None, true);
    message(zone, p, "Password set");
}

fn c_addallowed(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    if inv.params.is_empty() {
        message(zone, p, "You must specify a player name.");
        return;
    }
    let Some(file) = passwd(zone) else { return };
    if file.get("users", inv.params).is_some() {
        message(
            zone,
            p,
            &format!("{} has already set a local password.", inv.params),
        );
        return;
    }
    let by = zone
        .players
        .get(p)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    zone.cfg.set(
        &file,
        "users",
        inv.params,
        "any",
        Some(&format!("added by {by}")),
        true,
    );
    message(
        zone,
        p,
        &format!("Added {} to the allowed player list.", inv.params),
    );
}

fn c_set_local_password(zone: &mut Zone, inv: &Invocation<'_>) {
    let p = inv.sender;
    let Target::Player(t) = inv.target else {
        message(zone, p, "You must use this on a player.");
        return;
    };
    let Some(file) = passwd(zone) else { return };
    let Some((tname, hash)) = zone.players.get(t).map(|x| {
        (
            x.name.clone(),
            x.ext.get::<PwData>().and_then(|d| d.hash.clone()),
        )
    }) else {
        return;
    };
    let by = zone
        .players
        .get(p)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    if file.get("users", &tname).is_some() {
        message(
            zone,
            p,
            &format!("{tname} has already set a local password."),
        );
    } else if let Some(hash) = hash {
        zone.cfg.set(
            &file,
            "users",
            &tname,
            &hash,
            Some(&format!("added by {by}")),
            true,
        );
        message(zone, p, &format!("Set local password for {tname}."));
        message(
            zone,
            t,
            &format!("Your password has been set as a local password by {by}."),
        );
    } else {
        message(zone, p, "Hashed password missing.");
    }
}

const PASSWD_HELP: &str = "Module: auth_file\nTargets: none\nArgs: <new password>\n\
Changes your local server password. Note that this command only changes\n\
the password used by the auth_file authentication mechanism (used when the\n\
billing server is disconnected). This command does not involve the billing\n\
server.\n";

/// The `auth_file` module.
pub struct AuthFileModule;

impl Module for AuthFileModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        if passwd(zone).is_none() {
            return Err(ModuleError::Failed {
                module: "auth_file".into(),
                reason: "conf/passwd.conf not found".into(),
            });
        }
        zone.reg
            .provide::<dyn Auth>(me, Scope::Global, std::sync::Arc::new(AuthFile));
        zone.on::<NewPlayer>(me, Scope::Global, |z, ev| {
            if let Some(p) = z.players.get_mut(ev.player) {
                p.ext.remove::<PwData>();
            }
        });
        zone.add_command(me, "passwd", None, Some(PASSWD_HELP), c_passwd);
        zone.add_command(me, "local_password", None, Some(PASSWD_HELP), c_passwd);
        zone.add_command(
            me,
            "addallowed",
            None,
            Some("Module: auth_file\nTargets: none\nArgs: <player name>\nAdds a player to passwd.conf with no set password.\n"),
            c_addallowed,
        );
        zone.add_command(
            me,
            "set_local_password",
            None,
            Some("Module: auth_file\nTargets: player\nArgs: none\nSets a player's local password to the one they logged in with.\n"),
            c_set_local_password,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_layout() {
        // independent recomputation of the 56-byte layout
        let mut msg = [0u8; 56];
        msg[..3].copy_from_slice(b"bob");
        msg[24..30].copy_from_slice(b"secret");
        let want: String = Md5::digest(msg)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash_password("BoB", "secret"), want);
        assert_eq!(hash_password("x", "y").len(), 32);
        // over-long inputs are clipped, not panicking
        let _ = hash_password(&"n".repeat(100), &"p".repeat(100));
    }
}
