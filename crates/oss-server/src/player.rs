//! Players: the core per-connection state every module shares (ASSS `Player`).

use std::collections::VecDeque;
use std::net::SocketAddr;

use crate::arena::ArenaId;
use crate::registry::Extensions;
use crate::time::Ticks;

/// A player id. It is also the id sent on the wire, so ids are reused after a
/// player leaves (oldest-freed first, which delays reuse as long as possible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlayerId(pub u16);

/// Player lifecycle (ASSS `S_*` states). The core advances players through
/// these; modules can delay a transition with a hold where noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlayerStatus {
    /// Allocated, transport handshake not finished.
    Uninitialized,
    /// Connected, no login packet yet.
    Connected,
    /// Login received; the auth module must be asked.
    NeedAuth,
    /// Waiting for the auth module to answer.
    WaitAuth,
    /// Load global persistent data.
    NeedGlobalSync,
    /// Waiting for global persistent data.
    WaitGlobalSync1,
    /// Run the "player logged in" callbacks.
    DoGlobalCallbacks,
    /// Send the login response.
    SendLoginResponse,
    /// Logged in, not in an arena.
    LoggedIn,
    /// Entering an arena: load per-arena persistent data.
    DoFreqAndArenaSync,
    /// Waiting for per-arena data.
    WaitArenaSync1,
    /// Send arena entry packets and callbacks.
    ArenaRespAndCbs,
    /// In an arena.
    Playing,
    /// Leaving an arena.
    LeavingArena,
    /// Save per-arena persistent data.
    DoArenaSync2,
    /// Waiting for per-arena data to be saved.
    WaitArenaSync2,
    /// Leaving the zone.
    LeavingZone,
    /// Waiting for global data to be saved.
    WaitGlobalSync2,
    /// Gone; the slot is freed once the transport has drained.
    TimeWait,
}

/// What kind of client a player is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKind {
    /// Not known yet.
    Unknown,
    /// SubSpace 1.34/VIE-protocol clients (incl. Open SubSpace).
    Vie,
    /// Continuum.
    Continuum,
    /// A server-side fake player (bot, turret).
    Fake,
    /// A chat-protocol client.
    Chat,
}

/// A ship number: 0..=7 are Warbird..Shark, 8 is spectator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ship(pub u8);

impl Ship {
    /// Spectator mode.
    pub const SPEC: Ship = Ship(8);

    /// True for spectators.
    pub fn is_spec(self) -> bool {
        self.0 >= 8
    }
}

/// The last position a player reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    /// X in pixels.
    pub x: i16,
    /// Y in pixels.
    pub y: i16,
    /// X velocity.
    pub xspeed: i16,
    /// Y velocity.
    pub yspeed: i16,
    /// Rotation 0..40.
    pub rotation: u8,
    /// Current bounty.
    pub bounty: u16,
    /// Status bits (stealth, cloak, xradar, antiwarp, flash, safety, ufo, inert).
    pub status: u8,
    /// Energy (when sent).
    pub energy: u16,
    /// Client timestamp of the packet.
    pub time: u32,
}

/// One player.
pub struct Player {
    /// Wire id.
    pub id: PlayerId,
    /// Lifecycle state.
    pub status: PlayerStatus,
    /// Client type.
    pub kind: ClientKind,
    /// Display name (Latin-1 decoded).
    pub name: String,
    /// Squad name.
    pub squad: String,
    /// Remote address, for network players.
    pub addr: Option<SocketAddr>,
    /// Current arena.
    pub arena: Option<ArenaId>,
    /// Arena the player asked to enter, while entering.
    pub new_arena: Option<ArenaId>,
    /// Ship.
    pub ship: Ship,
    /// Frequency (team).
    pub freq: u16,
    /// Last reported position.
    pub pos: Position,
    /// When the player connected.
    pub connected_at: Ticks,
    /// Players with this flag don't get position packets relayed to them.
    pub ignore_weapons: bool,
    /// Logged in with a verified password (billing or password file).
    pub authenticated: bool,
    /// State to move to once the player is back at `LoggedIn` (used by kicks).
    pub when_logged_in: Option<PlayerStatus>,
    /// Client version from the login packet.
    pub client_version: u16,
    /// Machine id from the login packet.
    pub machine_id: u32,
    /// Permission id from the login packet.
    pub permission_id: u32,
    /// Screen size from the arena request.
    pub resolution: (i16, i16),
    /// Accepts voice messages.
    pub accept_audio: bool,
    /// Wants the obscenity filter.
    pub obscenity_filter: bool,
    /// Wants optional LVZ files.
    pub want_all_lvz: bool,
    /// Human-readable client description.
    pub client_name: String,
    /// Module-owned per-player data.
    pub ext: Extensions,
}

impl Player {
    fn new(id: PlayerId, kind: ClientKind, now: Ticks) -> Self {
        Self {
            id,
            status: PlayerStatus::Uninitialized,
            kind,
            name: String::new(),
            squad: String::new(),
            addr: None,
            arena: None,
            new_arena: None,
            ship: Ship::SPEC,
            freq: 0,
            pos: Position::default(),
            connected_at: now,
            ignore_weapons: false,
            authenticated: false,
            when_logged_in: None,
            client_version: 0,
            machine_id: 0,
            permission_id: 0,
            resolution: (0, 0),
            accept_audio: false,
            obscenity_filter: false,
            want_all_lvz: false,
            client_name: String::new(),
            ext: Extensions::default(),
        }
    }

    /// True if the player is in `arena` and fully entered.
    pub fn is_playing_in(&self, arena: ArenaId) -> bool {
        self.arena == Some(arena) && self.status == PlayerStatus::Playing
    }
}

/// The player table.
#[derive(Default)]
pub struct Players {
    slots: Vec<Option<Player>>,
    free: VecDeque<u16>,
    count: usize,
}

/// Highest player id handed out (ASSS caps ids well below the wire limit).
pub const MAX_PLAYERS: usize = 1024;

impl Players {
    /// Allocates a new player. Returns `None` when the table is full.
    pub fn alloc(&mut self, kind: ClientKind, now: Ticks) -> Option<PlayerId> {
        let idx = match self.free.pop_front() {
            Some(i) => i,
            None if self.slots.len() < MAX_PLAYERS => {
                self.slots.push(None);
                (self.slots.len() - 1) as u16
            }
            None => return None,
        };
        let id = PlayerId(idx);
        self.slots[usize::from(idx)] = Some(Player::new(id, kind, now));
        self.count += 1;
        Some(id)
    }

    /// Frees a player slot.
    pub fn remove(&mut self, id: PlayerId) -> Option<Player> {
        let p = self.slots.get_mut(usize::from(id.0))?.take()?;
        self.free.push_back(id.0);
        self.count -= 1;
        Some(p)
    }

    /// Looks up a player.
    pub fn get(&self, id: PlayerId) -> Option<&Player> {
        self.slots.get(usize::from(id.0))?.as_ref()
    }

    /// Looks up a player mutably.
    pub fn get_mut(&mut self, id: PlayerId) -> Option<&mut Player> {
        self.slots.get_mut(usize::from(id.0))?.as_mut()
    }

    /// Number of players.
    pub fn len(&self) -> usize {
        self.count
    }

    /// True when nobody is connected.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// All players.
    pub fn iter(&self) -> impl Iterator<Item = &Player> {
        self.slots.iter().flatten()
    }

    /// All players, mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Player> {
        self.slots.iter_mut().flatten()
    }

    /// Ids of all players (a snapshot, so the table can be modified while
    /// walking it).
    pub fn ids(&self) -> Vec<PlayerId> {
        self.iter().map(|p| p.id).collect()
    }

    /// Finds a player by name (ASCII case-insensitive).
    /// Players on their way out of the zone are skipped (ASSS `FindPlayer`).
    pub fn find_by_name(&self, name: &str) -> Option<&Player> {
        let leaving = |s: Option<PlayerStatus>| s.is_some_and(|s| s >= PlayerStatus::LeavingZone);
        self.iter().find(|p| {
            p.name.eq_ignore_ascii_case(name)
                && !leaving(Some(p.status))
                && !leaving(p.when_logged_in)
        })
    }

    /// Players fully in `arena`.
    pub fn in_arena(&self, arena: ArenaId) -> impl Iterator<Item = &Player> {
        self.iter().filter(move |p| p.is_playing_in(arena))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_reused_oldest_first() {
        let mut ps = Players::default();
        let now = Ticks(0);
        let a = ps.alloc(ClientKind::Vie, now);
        let b = ps.alloc(ClientKind::Vie, now);
        let c = ps.alloc(ClientKind::Vie, now);
        assert_eq!(
            (a, b, c),
            (Some(PlayerId(0)), Some(PlayerId(1)), Some(PlayerId(2)))
        );
        ps.remove(PlayerId(1));
        ps.remove(PlayerId(0));
        assert_eq!(ps.alloc(ClientKind::Vie, now), Some(PlayerId(1)));
        assert_eq!(ps.alloc(ClientKind::Vie, now), Some(PlayerId(0)));
        assert_eq!(ps.len(), 3);
        assert!(ps.remove(PlayerId(9)).is_none());
    }
}
