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
//!
//! # Names that are not listed, and the zone password
//!
//! ASSS and Subspace Server .NET both ship `AllowUnknown = yes`, and with it
//! a name that has no `[Users]` entry is let in with **any** password. That
//! is the historical behaviour of a public SubSpace zone and this module
//! keeps it, but it is no longer what an unconfigured zone does, because an
//! unconfigured zone is now something a stranger can reach in one command.
//!
//! Two keys, neither of which exists in ASSS, decide what happens to an
//! unlisted name when `AllowUnknown` is on:
//!
//! - **`[General] ZonePassword`** -- one shared secret for the whole zone.
//!   An unlisted name is accepted only if the password it logged in with
//!   equals this. The player is **not** marked authenticated: the zone
//!   password says which zone you are entitled to enter, not who you are, so
//!   it must never confer a staff group (see [`super::capman`], which keys
//!   staff groups on `authenticated`).
//! - **`[General] AllowAnyLogin`** -- restores the old behaviour explicitly:
//!   any name, any password, no zone password. Also reachable as
//!   `--insecure-allow-any-login`. A zone in this state logs a warning at
//!   every start.
//!
//! If neither key is set, [`AuthFileModule::post_load`] generates a zone
//! password, prints it once and installs it -- writing it into
//! `conf/passwd.conf` when the zone has a directory, and holding it for the
//! run when it does not.
//!
//! None of this runs when another module has taken over as the zone's
//! [`Auth`] provider, which is what [`super::billing_ssc`] does: a biller
//! **replaces** this module rather than layering over it, so `AllowUnknown`
//! and `ZonePassword` are not consulted at all while one is loaded.

use std::sync::Arc;

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

fn passwd(zone: &mut Zone) -> Option<Arc<crate::config::ConfigFile>> {
    zone.cfg.open(None, "passwd.conf").ok()
}

/// What the command line said about logins that no config file covers.
///
/// The runtime puts one of these in the zone's resources before the modules
/// load ([`crate::runtime::ServerBuilder::allow_any_login`] and
/// [`crate::runtime::ServerBuilder::zone_password`]). Absent, the default
/// means "the operator said nothing", which is the case
/// the module's post-load check exists for.
#[derive(Debug, Clone, Default)]
pub struct OpenLoginPolicy {
    /// `--insecure-allow-any-login`: accept any name with any password.
    pub allow_any_login: bool,
    /// `--zone-password <PASSWORD>`: use this rather than generating one.
    pub zone_password: Option<String>,
}

/// The `Arc` this module registered as the zone's [`Auth`], kept so
/// [`close_open_login`] can tell whether something else has replaced it.
struct OurAuth(Arc<dyn Auth>);

/// Characters a generated zone password is drawn from: 32 of them, so each
/// one carries exactly five bits, and none of the pairs a person reading a
/// log line aloud confuses (`0`/`O`, `1`/`l`/`I`).
const PW_ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// How many characters a generated zone password has (16 x 5 bits = 80).
const PW_LEN: usize = 16;

/// Draws a zone password from operating-system entropy.
///
/// Deliberately **not** [`crate::prng::Prng`]: that is the game's
/// ASSS-compatible generator, it is seeded and reproducible, and a zone
/// password drawn from it would be guessable by anyone who could watch the
/// zone play.
fn generate_zone_password() -> String {
    let entropy = oss_net::opensec::os_entropy();
    let mut buf = [0u8; PW_LEN];
    entropy(&mut buf);
    buf.iter()
        .map(|b| char::from(PW_ALPHABET[usize::from(b & 0x1f)]))
        .collect()
}

/// Whether `file` leaves an unlisted name able to log in with any password.
///
/// Note what is *not* here: entries in `[Users]`. A zone with
/// `AllowUnknown = yes` and a hundred listed players still lets a
/// hundred-and-first name in with any password, so a populated `[Users]`
/// does not close this door and must not be read as though it does.
fn accepts_any_login(file: &crate::config::ConfigFile) -> bool {
    file.get_int("General", "AllowUnknown", 1) != 0 && zone_password(file).is_none()
}

/// The configured zone password, if there is a non-empty one.
fn zone_password(file: &crate::config::ConfigFile) -> Option<&str> {
    file.get("General", "ZonePassword")
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Installs `password` as the zone password, in memory and -- when the zone
/// has a directory -- appended to `conf/passwd.conf` the way `?passwd`
/// already writes. With no directory (the bundled zone) the append is a
/// no-op and the value lasts for this run only.
fn install_zone_password(zone: &mut Zone, file: &Arc<crate::config::ConfigFile>, password: &str) {
    zone.cfg.set(
        file,
        "General",
        "ZonePassword",
        password,
        Some("zone password: any name may log in with this"),
        true,
    );
}

/// Closes the login that nobody chose, at the end of module loading.
///
/// The case this exists for is a zone nobody has configured: `AllowUnknown`
/// on (its ASSS default, so it needs no file to be true), no zone password,
/// no biller. Until 2026-11-16 that zone -- which is exactly what
/// `oss-server` with no arguments and `docker run` with no arguments produce
/// -- accepted any name with any password from anyone who could reach the
/// port. It no longer does.
///
/// What this does **not** do, on purpose:
///
/// - It does not change `AllowUnknown`'s default, which is `1` in ASSS
///   (`auth_file.c`, `cfghelp: General:AllowUnknown ... def: 1`) and in
///   Subspace Server .NET (`AuthFile.cs`, `Default = true`). Flipping it
///   would lock every unlisted player out of every zone directory written
///   for either server, which is an interoperability break and not the one
///   that was asked for.
/// - It does not touch a zone whose operator has decided something:
///   `AllowUnknown = no`, a `ZonePassword`, `AllowAnyLogin`, a biller.
/// - It does not refuse to start, and it does not bind to loopback. Both are
///   known answers to this shape of problem and both would stop `docker run`
///   from producing a zone anyone can join, which is what the bundled zone
///   is for.
fn close_open_login(zone: &mut Zone) {
    let policy = zone
        .res
        .get::<OpenLoginPolicy>()
        .cloned()
        .unwrap_or_default();

    // A biller replaces this module as the zone's authenticator rather than
    // layering over it (`billing_ssc` registers its own `Auth`, and the
    // registry keeps the last provider), so none of the keys below are read
    // while one is loaded. Say so rather than silently doing nothing.
    let ours = zone.res.get::<OurAuth>().map(|o| o.0.clone());
    let current = zone.iface::<dyn Auth>();
    let still_ours = match (&ours, &current) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    };
    if !still_ours {
        if policy.allow_any_login || policy.zone_password.is_some() {
            tracing::warn!(
                target: "auth_file",
                "another module is the zone's authenticator, so --insecure-allow-any-login and --zone-password do nothing here: that module decides who may log in"
            );
        }
        return;
    }

    let Some(file) = passwd(zone) else { return };

    // An explicit password from the command line wins over everything, so a
    // container can be restarted with the password its players already have.
    if let Some(pw) = policy.zone_password.clone() {
        let already = zone_password(&file) == Some(pw.as_str());
        install_zone_password(zone, &file, &pw);
        if !already {
            tracing::info!(
                target: "auth_file",
                "zone password set from the command line; unlisted names need it to log in"
            );
        }
        return;
    }

    if policy.allow_any_login || file.get_int("General", "AllowAnyLogin", 0) != 0 {
        tracing::warn!(
            target: "auth_file",
            "ANY NAME WITH ANY PASSWORD can log in to this zone: AllowAnyLogin is set. Nobody is authenticated, so nobody gets a staff group, but every name on this zone is a name anyone may claim. Remove it before this zone is reachable from anywhere you do not control."
        );
        return;
    }

    if !accepts_any_login(&file) {
        return;
    }

    let pw = generate_zone_password();
    install_zone_password(zone, &file, &pw);
    let persisted = zone.dir.is_some();
    // Printing a credential is deliberate here and is the only way the
    // operator can learn it: the same shape as the OpenSec public key this
    // server already logs at start, and as Elasticsearch's and Jenkins's
    // first-run passwords. It is printed when it is generated, which is once
    // whenever there is a file to keep it in.
    tracing::warn!(
        target: "auth_file",
        password = %pw,
        persisted,
        "no logins were configured for this zone, so one zone password was generated. Players log in with any name and THIS password. It is kept in conf/passwd.conf as [General] ZonePassword when the zone has a directory, and is new on every start when it does not. Pass --zone-password to choose your own, or set AllowAnyLogin to let any name in with any password."
    );
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
                } else if let Some(zp) = file.as_deref().and_then(zone_password) {
                    // The zone password admits any name, so it says which
                    // zone you may enter and nothing about who you are.
                    // `ad.authenticated` therefore stays false, exactly as
                    // it does for `AllowUnknown`, and capman gives these
                    // players the default group rather than a staff one.
                    if pass != zp {
                        ad.code = auth_code::BAD_PASSWORD;
                    }
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
        let iface: Arc<dyn Auth> = Arc::new(AuthFile);
        zone.reg
            .provide::<dyn Auth>(me, Scope::Global, iface.clone());
        zone.res.insert(OurAuth(iface));
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

    /// Runs once every module has loaded, which is the first moment it is
    /// knowable whether a biller has taken over as the zone's [`Auth`].
    fn post_load(&self, zone: &mut Zone, _me: Owner) {
        close_open_login(zone);
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<OurAuth>();
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
