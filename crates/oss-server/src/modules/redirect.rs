//! `redirect`: send players to another zone (ASSS `redirect.c`).
//!
//! S2C `0x3B` tells a Continuum client to connect elsewhere. Targets come
//! from `Redirects:<name>` in `global.conf` (`ip:port[:arena]`) or from a
//! literal `ip:port[:arena]`. A Continuum player who asks to `?go` to an arena
//! named in `[Redirects]` is redirected instead (see `arenaman`), and
//! `?redirect` sends its target to an alias or address.

use std::net::Ipv4Addr;

use oss_proto::wire::FixedStr;
use oss_proto::{Packet, s2c};

use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::PlayerId;
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

/// The redirect interface (ASSS `Iredirect`).
pub trait Redirect: Send + Sync {
    /// Sends `to` to `ip:port`, arena type and name as in C2S `0x01`.
    /// False if `ip` isn't an IPv4 address.
    fn raw_redirect(
        &self,
        zone: &mut Zone,
        to: &[PlayerId],
        ip: &str,
        port: u16,
        arena_type: i16,
        arena: &str,
    ) -> bool;

    /// Sends `to` to a `[Redirects]` alias or a literal `ip:port[:arena]`.
    /// False if `alias` names neither.
    fn alias_redirect(&self, zone: &mut Zone, to: &[PlayerId], alias: &str) -> bool;

    /// Handles a `?go` to `arena`: true if the player was redirected.
    fn arena_request(&self, zone: &mut Zone, p: PlayerId, arena: &str) -> bool;
}

/// Builds the S2C `0x3B` packet, or `None` if `ip` isn't an IPv4 address.
pub fn redirect_packet(ip: &str, port: u16, arena_type: i16, arena: &str) -> Option<Vec<u8>> {
    let ip: Ipv4Addr = ip.trim().parse().ok()?;
    let name = arena.as_bytes();
    let mut buf = Vec::with_capacity(29);
    s2c::Redirect {
        ip: ip.octets(),
        port,
        arena_type,
        arena: FixedStr::new(&name[..name.len().min(16)]),
        login_id: 0,
    }
    .encode(&mut buf);
    Some(buf)
}

/// Parses `ip:port[:arena]` into (ip, port, arena type, arena name) the way
/// ASSS does: a missing or bad port is 0; with an arena the type is -3
/// (named arena), otherwise -1 (any public arena).
pub fn parse_target(t: &str) -> Option<(String, u16, i16, String)> {
    let (ip, rest) = t.split_once(':')?;
    let ip: String = ip.chars().take(15).collect();
    let (port, arena) = match rest.split_once(':') {
        Some((p, a)) => (p, Some(a)),
        None => (rest, None),
    };
    let port = atoi(port);
    Some(match arena {
        Some(a) => (ip, port, -3, a.to_owned()),
        None => (ip, port, -1, String::new()),
    })
}

/// C `atoi`, truncated to 16 bits as ASSS's `u16` field does.
fn atoi(s: &str) -> u16 {
    let s = s.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for c in digits.bytes().take_while(u8::is_ascii_digit) {
        v = (v * 10 + i64::from(c - b'0')) & 0xFFFF_FFFF;
    }
    if neg {
        v = -v;
    }
    v as u16
}

fn looks_like_address(alias: &str) -> bool {
    alias.starts_with(|c: char| ('1'..='9').contains(&c))
        && alias.contains('.')
        && alias.contains(':')
}

struct RedirectImpl;

impl Redirect for RedirectImpl {
    fn raw_redirect(
        &self,
        zone: &mut Zone,
        to: &[PlayerId],
        ip: &str,
        port: u16,
        arena_type: i16,
        arena: &str,
    ) -> bool {
        let Some(pkt) = redirect_packet(ip, port, arena_type, arena) else {
            return false;
        };
        zone.send_many(to.iter().copied(), &pkt, SendFlags::RELIABLE);
        true
    }

    fn alias_redirect(&self, zone: &mut Zone, to: &[PlayerId], alias: &str) -> bool {
        let configured = zone.cfg.global().get("Redirects", alias).map(str::to_owned);
        let t = match configured {
            Some(t) => t,
            None if looks_like_address(alias) => alias.to_owned(),
            None => return false,
        };
        let Some((ip, port, ty, arena)) = parse_target(&t) else {
            return false;
        };
        self.raw_redirect(zone, to, &ip, port, ty, &arena)
    }

    fn arena_request(&self, zone: &mut Zone, p: PlayerId, arena: &str) -> bool {
        self.alias_redirect(zone, &[p], arena)
    }
}

fn c_redirect(zone: &mut Zone, inv: &Invocation<'_>) {
    let to = match inv.target {
        Target::Arena(_) | Target::None => vec![inv.sender],
        _ => super::staffcmd::targets(zone, inv),
    };
    if let Some(r) = zone.iface::<dyn Redirect>() {
        r.alias_redirect(zone, &to, inv.params.trim());
    }
}

/// The `redirect` module.
pub struct RedirectModule;

impl Module for RedirectModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Redirect>(me, Scope::Global, std::sync::Arc::new(RedirectImpl));
        zone.add_command(
            me,
            "redirect",
            None,
            Some(
                "Targets: any\nArgs: <redirect alias> | <ip>:<port>[:<arena>]\n\
                 Module: redirect\nRedirects the target to a different zone.\n",
            ),
            c_redirect,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("10.0.0.1:5000:pub"),
            Some(("10.0.0.1".into(), 5000, -3, "pub".into()))
        );
        assert_eq!(
            parse_target("10.0.0.1:5000"),
            Some(("10.0.0.1".into(), 5000, -1, String::new()))
        );
        assert_eq!(parse_target("nocolon"), None);
        assert_eq!(atoi("70000"), 70000u32 as u16);
        assert!(looks_like_address("10.0.0.1:5000"));
        assert!(!looks_like_address("0.0.0.1:5000"));
        let pkt = redirect_packet("1.2.3.4", 0x1234, -3, "arenaname-longer-than-16").unwrap();
        assert_eq!(pkt.len(), 29);
        assert_eq!(&pkt[..7], &[0x3B, 1, 2, 3, 4, 0x34, 0x12]);
        assert_eq!(&pkt[9..25], b"arenaname-longer");
        assert!(redirect_packet("host.example", 1, -1, "").is_none());
    }
}
