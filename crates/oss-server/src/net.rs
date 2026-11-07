//! The boundary between game logic and the transport.
//!
//! Modules never touch sockets. They queue game packets for players in the
//! zone's [`Outbox`]; a [`Transport`] (the SubSpace core protocol from
//! `oss-net`, or a loopback in tests) turns them into datagrams and feeds
//! decoded game packets back through [`crate::zone::Zone::handle_packet`].
//!
//! The outbox stores packet bytes once in a shared buffer, so sending one
//! packet to every player in an arena costs one copy, not one per recipient,
//! and steady-state sending does not allocate.

use std::net::SocketAddr;

use crate::player::PlayerId;
use crate::zone::Zone;

/// How a packet should be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendFlags {
    /// Use the reliable channel (ordered, retransmitted).
    pub reliable: bool,
    /// May be dropped under bandwidth pressure (e.g. far-away positions).
    pub droppable: bool,
    /// Send ahead of normal traffic.
    pub urgent: bool,
}

impl SendFlags {
    /// Unreliable, normal priority.
    pub const UNRELIABLE: SendFlags = SendFlags {
        reliable: false,
        droppable: false,
        urgent: false,
    };
    /// Reliable, normal priority.
    pub const RELIABLE: SendFlags = SendFlags {
        reliable: true,
        droppable: false,
        urgent: false,
    };
    /// Unreliable and droppable.
    pub const DROPPABLE: SendFlags = SendFlags {
        reliable: false,
        droppable: true,
        urgent: false,
    };
}

/// One queued packet.
#[derive(Debug, Clone, Copy)]
pub struct Outgoing {
    /// Recipient.
    pub to: PlayerId,
    /// Delivery flags.
    pub flags: SendFlags,
    start: u32,
    len: u32,
}

/// Why the server is dropping a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnect {
    /// The player.
    pub player: PlayerId,
    /// Reason, for logs.
    pub reason: String,
}

/// Game packets waiting for the transport.
#[derive(Default)]
pub struct Outbox {
    buf: Vec<u8>,
    items: Vec<Outgoing>,
    disconnects: Vec<Disconnect>,
}

impl Outbox {
    fn push_bytes(&mut self, data: &[u8]) -> (u32, u32) {
        let start = self.buf.len() as u32;
        self.buf.extend_from_slice(data);
        (start, data.len() as u32)
    }

    /// Queues `data` for one player.
    pub fn send(&mut self, to: PlayerId, data: &[u8], flags: SendFlags) {
        if data.is_empty() {
            return;
        }
        let (start, len) = self.push_bytes(data);
        self.items.push(Outgoing {
            to,
            flags,
            start,
            len,
        });
    }

    /// Queues the same `data` for several players, storing it once.
    pub fn send_many(
        &mut self,
        to: impl IntoIterator<Item = PlayerId>,
        data: &[u8],
        flags: SendFlags,
    ) {
        if data.is_empty() {
            return;
        }
        let (start, len) = self.push_bytes(data);
        let before = self.items.len();
        self.items.extend(to.into_iter().map(|to| Outgoing {
            to,
            flags,
            start,
            len,
        }));
        if self.items.len() == before {
            self.buf.truncate(start as usize);
        }
    }

    /// Asks the transport to drop a player's connection.
    pub fn disconnect(&mut self, player: PlayerId, reason: impl Into<String>) {
        self.disconnects.push(Disconnect {
            player,
            reason: reason.into(),
        });
    }

    /// The bytes of a queued packet.
    pub fn bytes(&self, item: &Outgoing) -> &[u8] {
        let s = item.start as usize;
        self.buf.get(s..s + item.len as usize).unwrap_or(&[])
    }

    /// Queued packets, in order.
    pub fn items(&self) -> &[Outgoing] {
        &self.items
    }

    /// Queued disconnects.
    pub fn disconnects(&self) -> &[Disconnect] {
        &self.disconnects
    }

    /// True if nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.disconnects.is_empty()
    }

    /// Empties the queue, keeping its capacity.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.items.clear();
        self.disconnects.clear();
    }
}

/// Sends raw datagrams. Implemented over the UDP socket by the runtime.
pub trait DatagramIo: Send + Sync {
    /// Sends one datagram; failures (full buffers) are dropped like UDP loss.
    fn send_to(&self, data: &[u8], to: SocketAddr);
}

/// Turns datagrams into game packets and back. Owned by the game loop and
/// driven on the game thread, so it may borrow the zone mutably.
pub trait Transport: Send {
    /// A datagram arrived from `from`.
    fn on_datagram(&mut self, zone: &mut Zone, io: &dyn DatagramIo, from: SocketAddr, data: &[u8]);

    /// Moves the zone's outbox onto the wire. Called after each batch of
    /// input and every tick.
    fn flush(&mut self, zone: &mut Zone, io: &dyn DatagramIo);

    /// Once per tick: retransmissions, keepalives, timeouts.
    fn tick(&mut self, zone: &mut Zone, io: &dyn DatagramIo);

    /// The server is stopping: say goodbye to every connection.
    fn shutdown(&mut self, zone: &mut Zone, io: &dyn DatagramIo) {
        self.flush(zone, io);
    }
}

/// A transport with no network at all: queued packets are kept per player
/// for inspection. Used by tests and by tools that drive the zone directly.
#[derive(Default)]
pub struct Loopback {
    sent: std::collections::HashMap<PlayerId, Vec<(Vec<u8>, SendFlags)>>,
    dropped: Vec<Disconnect>,
}

impl Loopback {
    /// Moves the zone's outbox into the per-player mailboxes.
    pub fn collect(&mut self, zone: &mut Zone) {
        let out = zone.take_outbox();
        for item in out.items() {
            self.sent
                .entry(item.to)
                .or_default()
                .push((out.bytes(item).to_vec(), item.flags));
        }
        self.dropped.extend(out.disconnects().iter().cloned());
        zone.return_outbox(out);
    }

    /// Takes everything sent to `player` so far.
    pub fn take(&mut self, player: PlayerId) -> Vec<(Vec<u8>, SendFlags)> {
        self.sent.remove(&player).unwrap_or_default()
    }

    /// Takes the disconnects requested so far.
    pub fn take_disconnects(&mut self) -> Vec<Disconnect> {
        std::mem::take(&mut self.dropped)
    }
}

impl Transport for Loopback {
    fn on_datagram(&mut self, _: &mut Zone, _: &dyn DatagramIo, _: SocketAddr, _: &[u8]) {}

    fn flush(&mut self, zone: &mut Zone, _: &dyn DatagramIo) {
        self.collect(zone);
    }

    fn tick(&mut self, _: &mut Zone, _: &dyn DatagramIo) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_stores_bytes_once() {
        let mut o = Outbox::default();
        o.send_many(
            [PlayerId(1), PlayerId(2)],
            &[1, 2, 3],
            SendFlags::UNRELIABLE,
        );
        o.send(PlayerId(3), &[9], SendFlags::RELIABLE);
        o.send_many([], &[7, 7], SendFlags::UNRELIABLE);
        assert_eq!(o.buf.len(), 4);
        let got: Vec<_> = o
            .items()
            .iter()
            .map(|i| (i.to, o.bytes(i).to_vec()))
            .collect();
        assert_eq!(
            got,
            vec![
                (PlayerId(1), vec![1, 2, 3]),
                (PlayerId(2), vec![1, 2, 3]),
                (PlayerId(3), vec![9])
            ]
        );
        o.clear();
        assert!(o.is_empty());
    }
}
