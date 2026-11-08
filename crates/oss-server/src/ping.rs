//! The zone "ping" protocol on the game port + 1 (ASSS `net.c`
//! `handle_ping_packet`, `doc/ping.txt`), answered by the I/O task from a
//! snapshot the game loop refreshes periodically.
//!
//! * 4-byte request `t`: reply `[total players: u32][t]`.
//! * 8-byte request `[t][options]`: reply `[t][options answered]` followed by
//!   the global summary `[total][playing]` (option 1) and/or the arena summary
//!   (option 2: `name\0 total:u16 playing:u16` repeated, then `\0`).

use crate::arena::ArenaStatus;
use crate::player::PlayerStatus;
use crate::zone::Zone;

const GLOBAL_SUMMARY: u32 = 0x01;
const ARENA_SUMMARY: u32 = 0x02;
/// Largest ping reply (ASSS `MAXPACKET`).
const MAX_PACKET: usize = 512;

/// Population data served to pingers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PingSnapshot {
    /// Players in arenas.
    pub total: u32,
    /// Players in arenas and in a ship.
    pub playing: u32,
    /// Encoded arena summary, including the terminating NUL.
    pub arenas: Vec<u8>,
    /// Players in arenas, by arena base name (ASSS counts a virtual
    /// server's population as its `ConnectAs` arenas').
    pub by_basename: std::collections::HashMap<String, u32>,
}

impl PingSnapshot {
    /// Builds a snapshot from the zone (ASSS `GetPopulationSummary`).
    pub fn build(zone: &Zone) -> Self {
        let mut s = PingSnapshot::default();
        let mut per_arena: std::collections::HashMap<crate::arena::ArenaId, (u32, u32)> =
            std::collections::HashMap::new();
        for p in zone.players.iter() {
            if p.status != PlayerStatus::Playing {
                continue;
            }
            let Some(a) = p.arena else { continue };
            let e = per_arena.entry(a).or_default();
            e.0 += 1;
            s.total += 1;
            if !p.ship.is_spec() {
                e.1 += 1;
                s.playing += 1;
            }
        }
        let mut arenas: Vec<_> = zone.arenas.iter().collect();
        arenas.sort_by_key(|a| a.id);
        for a in arenas {
            if s.arenas.len() > 480 {
                break;
            }
            if a.status != ArenaStatus::Running || a.name.starts_with('#') {
                continue;
            }
            let (total, playing) = per_arena.get(&a.id).copied().unwrap_or_default();
            *s.by_basename.entry(a.basename.clone()).or_default() += total;
            s.arenas
                .extend_from_slice(&crate::config::to_latin1(&a.name));
            s.arenas.push(0);
            s.arenas.extend_from_slice(&(total as u16).to_le_bytes());
            s.arenas.extend_from_slice(&(playing as u16).to_le_bytes());
        }
        s.arenas.push(0);
        s
    }

    /// The reply to a ping request, or `None` for malformed requests.
    pub fn reply(&self, req: &[u8]) -> Option<Vec<u8>> {
        self.reply_for(req, None)
    }

    /// Population of a virtual server (`ConnectAs`), or of the whole zone.
    pub fn population(&self, connect_as: Option<&str>) -> u32 {
        match connect_as {
            Some(c) => self.by_basename.get(c).copied().unwrap_or(0),
            None => self.total,
        }
    }

    /// The reply on a listen socket presenting virtual server `connect_as`:
    /// a 4-byte ping gets that server's population (ASSS `ld->total`).
    pub fn reply_for(&self, req: &[u8], connect_as: Option<&str>) -> Option<Vec<u8>> {
        match req.len() {
            4 => {
                let mut out = Vec::with_capacity(8);
                out.extend_from_slice(&self.population(connect_as).to_le_bytes());
                out.extend_from_slice(req);
                Some(out)
            }
            8 => {
                let opts_in = u32::from_le_bytes([req[4], req[5], req[6], req[7]]);
                let mut opts_out = 0u32;
                let mut out = Vec::with_capacity(MAX_PACKET);
                out.extend_from_slice(&req[..4]);
                out.extend_from_slice(&[0; 4]);
                if opts_in & GLOBAL_SUMMARY != 0 && out.len() + 8 < MAX_PACKET {
                    out.extend_from_slice(&self.total.to_le_bytes());
                    out.extend_from_slice(&self.playing.to_le_bytes());
                    opts_out |= GLOBAL_SUMMARY;
                }
                if opts_in & ARENA_SUMMARY != 0 && out.len() + self.arenas.len() < MAX_PACKET {
                    out.extend_from_slice(&self.arenas);
                    opts_out |= ARENA_SUMMARY;
                }
                out[4..8].copy_from_slice(&opts_out.to_le_bytes());
                Some(out)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies() {
        let s = PingSnapshot {
            total: 7,
            playing: 3,
            arenas: b"0\0\x07\0\x03\0\0".to_vec(),
            by_basename: [("duel".to_owned(), 2)].into_iter().collect(),
        };
        assert_eq!(s.reply(&[1, 2, 3, 4]), Some(vec![7, 0, 0, 0, 1, 2, 3, 4]));
        // a virtual server answers with its own population
        assert_eq!(
            s.reply_for(&[1, 2, 3, 4], Some("duel")),
            Some(vec![2, 0, 0, 0, 1, 2, 3, 4])
        );
        assert_eq!(s.population(Some("other")), 0);
        let r = s.reply(&[1, 2, 3, 4, 3, 0, 0, 0]).unwrap_or_default();
        assert_eq!(&r[..8], &[1, 2, 3, 4, 3, 0, 0, 0]);
        assert_eq!(&r[8..16], &[7, 0, 0, 0, 3, 0, 0, 0]);
        assert_eq!(&r[16..], b"0\0\x07\0\x03\0\0");
        assert_eq!(s.reply(&[1, 2, 3]), None);
    }
}
