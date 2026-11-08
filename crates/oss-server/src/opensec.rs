//! OpenSec for the server: the long-term key and the acceptor setup.
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

use oss_net::opensec::{Entropy, OpenSecAcceptor, ServerKey, os_entropy};

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

/// The OpenSec acceptor for this zone, or `None` if `[OpenSec] Enabled` is 0.
/// Also records [`ServerPublicKey`].
pub fn acceptor(zone: &mut Zone) -> Option<OpenSecAcceptor> {
    if zone.cfg.global().get_int("OpenSec", "Enabled", 1) == 0 {
        return None;
    }
    let entropy = os_entropy();
    let key = load_or_create(zone, &entropy);
    let public = ServerPublicKey(key.public());
    tracing::info!(target: "opensec", public_key = %public.hex(), "OpenSec enabled");
    zone.res.insert(public);
    let acceptor = OpenSecAcceptor::new(Some(key), entropy);
    if zone.cfg.global().get_int("OpenSec", "ClientIdentities", 1) != 0 {
        Some(acceptor.with_client_identities())
    } else {
        Some(acceptor)
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
