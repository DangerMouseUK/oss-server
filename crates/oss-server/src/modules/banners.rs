//! `banners`: player banners (ASSS `banners.c`).
//!
//! A spectator sets a banner with C2S `0x19`; the biller can also supply one
//! at login. A banner is held until the player is playing in an arena, then
//! checked against the optional [`BannerPolicy`] (everyone may have one when
//! no policy is registered, as in ASSS) and sent to the arena as S2C `0x1F`.
//! Players entering an arena receive the banners of everyone already there.

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::s2c;
use oss_proto::wire::Raw;

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::chat::message;
use super::core::{Action, PlayerAction};

/// Bytes in a banner: 12x8 pixels of palette indices.
pub const BANNER_LEN: usize = 96;

/// A banner image.
pub type Banner = [u8; BANNER_LEN];

/// Decides who may show a banner (ASSS `CheckBanner`). Register one as a
/// global interface to restrict banners, e.g. by points or billing status.
pub trait BannerPolicy: Send + Sync {
    /// True to allow `p`'s banner.
    fn allowed(&self, zone: &Zone, p: PlayerId) -> bool;
}

/// The banners interface (ASSS `Ibanners`).
pub trait Banners: Send + Sync {
    /// Sets a player's banner. `from_biller` banners don't raise [`BannerSet`].
    fn set_banner(&self, zone: &mut Zone, p: PlayerId, banner: &Banner, from_biller: bool);
}

/// A player's banner went out to their arena (ASSS `CB_SET_BANNER`). Not
/// raised for banners from the biller or for re-sends on arena entry.
#[derive(Debug, Clone)]
pub struct BannerSet {
    /// The player.
    pub player: PlayerId,
    /// Their arena.
    pub arena: ArenaId,
    /// The banner.
    pub banner: Banner,
}

impl Event for BannerSet {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Status {
    #[default]
    None,
    Pending,
    Allowed,
}

#[derive(Debug, Clone, Copy)]
struct BannerData {
    banner: Banner,
    status: Status,
}

impl Default for BannerData {
    fn default() -> Self {
        Self {
            banner: [0; BANNER_LEN],
            status: Status::None,
        }
    }
}

/// A player's banner, if it has been allowed and shown.
pub fn banner(zone: &Zone, p: PlayerId) -> Option<Banner> {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<BannerData>())
        .filter(|d| d.status == Status::Allowed)
        .map(|d| d.banner)
}

fn encode(pid: PlayerId, banner: &Banner) -> Vec<u8> {
    let mut buf = Vec::with_capacity(BANNER_LEN + 3);
    s2c::Banner {
        pid: pid.0 as i16,
        banner: Raw(*banner),
    }
    .encode(&mut buf);
    buf
}

fn check_and_send(zone: &mut Zone, p: PlayerId, call_others: bool) {
    let Some(mut d) = zone
        .players
        .get(p)
        .and_then(|x| x.ext.get::<BannerData>())
        .copied()
    else {
        return;
    };
    if d.status == Status::Pending {
        let ok = zone
            .iface::<dyn BannerPolicy>()
            .is_none_or(|pol| pol.allowed(zone, p));
        if ok {
            d.status = Status::Allowed;
            if let Some(x) = zone.players.get_mut(p) {
                x.ext.insert(d);
            }
        } else {
            tracing::debug!(target: "banners", pid = p.0, "denied permission to use a banner");
        }
    }
    if d.status != Status::Allowed {
        return;
    }
    let Some(arena) = zone.players.get(p).and_then(|x| x.arena) else {
        return;
    };
    let pkt = encode(p, &d.banner);
    zone.send_to_arena(arena, None, &pkt, SendFlags::RELIABLE);
    if call_others {
        zone.fire(&BannerSet {
            player: p,
            arena,
            banner: d.banner,
        });
    }
}

/// Sets a player's banner (see [`Banners::set_banner`]).
pub fn set_banner(zone: &mut Zone, p: PlayerId, banner: &Banner, from_biller: bool) {
    let Some(x) = zone.players.get_mut(p) else {
        return;
    };
    x.ext.insert(BannerData {
        banner: *banner,
        status: Status::Pending,
    });
    // held until the player is playing; checked on arena entry otherwise
    if x.status == PlayerStatus::Playing {
        check_and_send(zone, p, !from_biller);
    }
}

fn on_banner(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if pkt.len() != BANNER_LEN + 1 {
        tracing::warn!(target: "banners", pid = p.0, len = pkt.len(), "bad size for banner packet");
        return;
    }
    let Ok(req) = c2s::SetBanner::decode(pkt) else {
        return;
    };
    let Some(pl) = zone.players.get(p) else {
        return;
    };
    if pl.arena.is_none() {
        tracing::warn!(target: "banners", pid = p.0, "tried to set a banner from outside an arena");
        return;
    }
    if !pl.ship.is_spec() {
        message(zone, p, "You must be in spectator mode to set a banner.");
        tracing::info!(target: "banners", pid = p.0, "tried to set a banner while in a ship");
        return;
    }
    set_banner(zone, p, &req.banner.0, false);
    tracing::trace!(target: "banners", pid = p.0, "set banner");
}

fn on_enter(zone: &mut Zone, p: PlayerId, arena: ArenaId) {
    check_and_send(zone, p, false);
    let others: Vec<(PlayerId, Banner)> = zone
        .players
        .in_arena(arena)
        .filter(|x| x.id != p && x.status == PlayerStatus::Playing)
        .filter_map(|x| {
            x.ext
                .get::<BannerData>()
                .filter(|d| d.status == Status::Allowed)
                .map(|d| (x.id, d.banner))
        })
        .collect();
    for (id, b) in others {
        let pkt = encode(id, &b);
        zone.send(p, &pkt, SendFlags::RELIABLE);
    }
}

struct BannersImpl;

impl Banners for BannersImpl {
    fn set_banner(&self, zone: &mut Zone, p: PlayerId, banner: &Banner, from_biller: bool) {
        set_banner(zone, p, banner, from_biller);
    }
}

/// The `banners` module.
pub struct BannersModule;

impl Module for BannersModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::BANNER, on_banner);
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action == Action::EnterArena
                && let Some(a) = ev.arena
            {
                on_enter(z, ev.player, a);
            }
        });
        zone.reg
            .provide::<dyn Banners>(me, Scope::Global, std::sync::Arc::new(BannersImpl));
        Ok(())
    }
}
