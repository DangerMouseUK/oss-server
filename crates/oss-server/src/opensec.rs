//! OpenSec for the server: the long-term key and the mechanism built from it.
//!
//! OpenSec (oss-core `docs/protocol/opensec.md`) gives connections
//! ChaCha20-Poly1305 encryption with forward secrecy, and lets clients
//! authenticate the server by pinning its X25519 public key. Clients that
//! don't speak it fall back to VIE, so it runs alongside the other ciphers.
//!
//! `[OpenSec]` in `global.conf`:
//! - `Enabled` (default 1): accept OpenSec connections.
//! - `ClientIdentities` (default 1): accept clients' long-term identity keys
//!   (`CLIENT_AUTH`), which bans can then match (`Player::identity_key`,
//!   known once the login arrives).
//! - `KeyFile` (default `data/opensec.key`, under the zone directory): the
//!   server's secret key as 64 hex digits. It is created on first start; keep
//!   it private and keep it, since clients that pinned the public key will
//!   refuse a server whose key changed. Zones without a directory get a new key
//!   each run.
//!
//! The public key is logged at start, shown by `?serverkey`, and available
//! as [`ServerPublicKey`].

use std::path::PathBuf;

use oss_net::mechanism::{Mechanism, OpenSecSetup};
use oss_net::opensec::{Entropy, ServerKey, os_entropy};

use crate::zone::Zone;

/// The server's OpenSec public key (what clients pin), when OpenSec is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerPublicKey(pub [u8; 32]);

impl ServerPublicKey {
    /// Lowercase hex.
    pub fn hex(&self) -> String {
        hex(&self.0)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn key_path(zone: &Zone) -> Option<PathBuf> {
    let dir = zone.dir.as_ref()?;
    let name = zone
        .cfg
        .global()
        .get("OpenSec", "KeyFile")
        .unwrap_or("data/opensec.key")
        .to_owned();
    Some(dir.root().join(name))
}

/// Loads the server key, creating and saving one if there is none.
fn load_or_create(zone: &Zone, entropy: &Entropy) -> ServerKey {
    let Some(path) = key_path(zone) else {
        tracing::info!(target: "opensec", "no zone directory: using a new server key for this run");
        return ServerKey::generate(entropy);
    };
    if let Ok(text) = std::fs::read_to_string(&path) {
        match unhex(&text) {
            Some(secret) => return ServerKey::from_secret(secret),
            None => {
                tracing::error!(target: "opensec", file = %path.display(), "bad key file; using a new key for this run (not saving it)");
                return ServerKey::generate(entropy);
            }
        }
    }
    let key = ServerKey::generate(entropy);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match write_private(&path, &format!("{}\n", hex(&key.secret_bytes()))) {
        Ok(()) => {
            tracing::info!(target: "opensec", file = %path.display(), "created a new server key");
        }
        Err(e) => {
            tracing::error!(target: "opensec", file = %path.display(), "can't save the server key: {e}");
        }
    }
    key
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(text.as_bytes())
}

/// Whether `[OpenSec] Enabled` is on (it is unless set to 0).
pub fn enabled(zone: &Zone) -> bool {
    zone.cfg.global().get_int("OpenSec", "Enabled", 1) != 0
}

/// The OpenSec mechanism for this zone, to register with the zone's others.
///
/// With `[OpenSec] Enabled` on, this loads the server key (creating and saving
/// one if there is none), logs its public half, records [`ServerPublicKey`],
/// and takes client identity keys unless `ClientIdentities` is 0.
///
/// With it off, the mechanism is still returned, because OpenSec is built into
/// this program and the supported line says so, but it has no server key:
/// nothing is read or written, no [`ServerPublicKey`] is recorded, and the
/// zone's policy leaves the mechanism out, so it is never served.
pub fn mechanism(zone: &mut Zone) -> Mechanism {
    let entropy = os_entropy();
    if !enabled(zone) {
        return Mechanism::opensec(OpenSecSetup::new(entropy));
    }
    let key = load_or_create(zone, &entropy);
    let public = ServerPublicKey(key.public());
    tracing::info!(target: "opensec", public_key = %public.hex(), "OpenSec enabled");
    zone.res.insert(public);
    let setup = OpenSecSetup::new(entropy).server_key(key);
    if zone.cfg.global().get_int("OpenSec", "ClientIdentities", 1) != 0 {
        Mechanism::opensec(setup.client_identities())
    } else {
        Mechanism::opensec(setup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let k = [0xABu8; 32];
        assert_eq!(unhex(&hex(&k)), Some(k));
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(&format!(" {}\n", hex(&k))), Some(k));
    }
}
