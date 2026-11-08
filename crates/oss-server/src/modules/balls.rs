//! `balls`: soccer balls (ASSS `balls.c`).
//!
//! `Soccer:BallCount` balls spawn around `Soccer:SpawnX/SpawnY` (or up to
//! four spawn points `SpawnX1..3`, used by ball id mod 2 or 4) within
//! `Soccer:SpawnRadius`. Clients pick balls up (C2S `0x20`, which must name
//! the ball's current timestamp), fire them (C2S `0x1F`) and report goals
//! (C2S `0x21`); the server keeps the authoritative state and broadcasts
//! positions (S2C `0x2E`) every `Soccer:SendTime` ticks. After a goal the
//! ball respawns, at once or after `Soccer:GoalDelay`. A carrier who dies,
//! changes ship or team, or leaves drops the ball where they are.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::s2c::BallPosition;

use crate::arena::{ArenaId, ArenaStatus};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::prng::Prng;
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};
use super::core::{Action, PlayerAction};
use super::game::{Kill, ShipFreqChange};

/// Most balls an arena can have.
pub const MAX_BALLS: usize = 255;

/// Where a ball is (ASSS `ballstate_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BallState {
    /// Doesn't exist.
    #[default]
    None,
    /// On the map (or flying).
    OnMap,
    /// Carried.
    Carried,
    /// Waiting to respawn at `time`.
    Waiting,
}

/// One ball (ASSS `struct BallData`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ball {
    /// State.
    pub state: BallState,
    /// X pixel.
    pub x: i16,
    /// Y pixel.
    pub y: i16,
    /// X velocity.
    pub x_speed: i16,
    /// Y velocity.
    pub y_speed: i16,
    /// Carrier, or the last player to touch it.
    pub carrier: Option<PlayerId>,
    /// Frequency of that player.
    pub freq: i16,
    /// When it was last fired (0 while carried; respawn time while waiting).
    pub time: u32,
}

/// ASSS `CB_BALLPICKUP`.
#[derive(Debug, Clone, Copy)]
pub struct BallPickup {
    /// Arena.
    pub arena: ArenaId,
    /// Player.
    pub player: PlayerId,
    /// Ball id.
    pub ball: usize,
}

impl Event for BallPickup {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_BALLFIRE` (also raised when a carrier drops the ball).
#[derive(Debug, Clone, Copy)]
pub struct BallFire {
    /// Arena.
    pub arena: ArenaId,
    /// Player.
    pub player: PlayerId,
    /// Ball id.
    pub ball: usize,
}

impl Event for BallFire {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// ASSS `CB_GOAL`.
#[derive(Debug, Clone, Copy)]
pub struct Goal {
    /// Arena.
    pub arena: ArenaId,
    /// Scorer.
    pub player: PlayerId,
    /// Ball id.
    pub ball: usize,
    /// Goal tile x.
    pub x: i16,
    /// Goal tile y.
    pub y: i16,
}

impl Event for Goal {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

#[derive(Default)]
struct ArenaBalls {
    balls: Vec<Ball>,
    send_time: u64,
    last_sent: u64,
    spawn: [(i32, i32, i32); 4],
    goal_delay: u64,
    death_goal: bool,
}

fn data(zone: &mut Zone, a: ArenaId) -> Option<&mut ArenaBalls> {
    zone.arenas.get_mut(a)?.ext.get_mut::<ArenaBalls>()
}

/// A copy of an arena's balls.
pub fn balls(zone: &Zone, a: ArenaId) -> Vec<Ball> {
    zone.arenas
        .get(a)
        .and_then(|a| a.ext.get::<ArenaBalls>())
        .map(|d| d.balls.clone())
        .unwrap_or_default()
}

fn send_ball(zone: &mut Zone, a: ArenaId, bid: usize) {
    let Some(b) = data(zone, a).and_then(|d| d.balls.get(bid).copied()) else {
        return;
    };
    let time = match b.state {
        BallState::Carried => 0,
        BallState::OnMap => b.time,
        _ => return,
    };
    let mut buf = Vec::with_capacity(16);
    BallPosition {
        ball_id: bid as u8,
        x: b.x,
        y: b.y,
        x_speed: b.x_speed,
        y_speed: b.y_speed,
        pid: b.carrier.map_or(-1, |p| p.0 as i16),
        time,
    }
    .encode(&mut buf);
    zone.send_to_arena(a, None, &buf, SendFlags::UNRELIABLE);
}

/// ASSS `PlaceBall`.
pub fn place_ball(zone: &mut Zone, a: ArenaId, bid: usize, ball: Ball) {
    let Some(d) = data(zone, a) else { return };
    let Some(slot) = d.balls.get_mut(bid) else {
        return;
    };
    *slot = ball;
    send_ball(zone, a, bid);
    tracing::debug!(target: "balls", arena = a.0, bid, x = ball.x, y = ball.y, "ball placed");
}

/// Makes a ball unpickupable for clients that saw it (ASSS `phase_ball`).
fn phase_ball(zone: &mut Zone, a: ArenaId, bid: usize) {
    let Some(b) = data(zone, a).and_then(|d| d.balls.get_mut(bid)) else {
        return;
    };
    *b = Ball {
        state: BallState::OnMap,
        x: 30000,
        y: 30000,
        time: 0,
        ..Ball::default()
    };
    send_ball(zone, a, bid);
}

/// ASSS `SpawnBall`.
pub fn spawn_ball(zone: &mut Zone, a: ArenaId, bid: usize) {
    let Some(spawn) = data(zone, a).map(|d| d.spawn) else {
        return;
    };
    let idx = if spawn[3].0 != 0 && spawn[3].1 != 0 {
        bid % 4
    } else if spawn[1].0 != 0 && spawn[1].1 != 0 {
        bid % 2
    } else {
        0
    };
    let (cx, cy, rad) = spawn[idx];
    let rng = zone.res.get_or_default::<Prng>();
    let uniform = |r: &mut Prng| f64::from(r.next_u32()) / 4_294_967_296.0;
    let rr = uniform(rng) * f64::from(rad);
    let ang = uniform(rng) * std::f64::consts::TAU;
    let fine = rng.next_u32() & 0xff;
    let mut x = (cx + (rr * ang.cos()) as i32).rem_euclid(1024);
    let mut y = (cy + (rr * ang.sin()) as i32).rem_euclid(1024);
    if let Some(m) = super::mapdata::arena_map(zone, a) {
        (x, y) = m.find_empty_tile_near(x, y);
    }
    let x = (x << 4) | (fine / 16) as i32;
    let y = (y << 4) | (fine % 16) as i32;
    let now = zone.now().wire();
    place_ball(
        zone,
        a,
        bid,
        Ball {
            state: BallState::OnMap,
            x: x as i16,
            y: y as i16,
            time: now,
            ..Ball::default()
        },
    );
}

/// ASSS `SetBallCount`.
pub fn set_ball_count(zone: &mut Zone, a: ArenaId, count: usize) {
    if count > MAX_BALLS {
        return;
    }
    let Some(old) = data(zone, a).map(|d| d.balls.len()) else {
        return;
    };
    for bid in count..old {
        phase_ball(zone, a, bid);
    }
    if let Some(d) = data(zone, a) {
        d.balls.resize(count, Ball::default());
    }
    for bid in old..count {
        spawn_ball(zone, a, bid);
    }
}

/// ASSS `EndGame`: removes every ball and respawns them after
/// `Soccer:NewGameDelay` (negative = random up to that many ticks).
pub fn end_game(zone: &mut Zone, a: ArenaId) {
    let Some(n) = data(zone, a).map(|d| d.balls.len()) else {
        return;
    };
    let delay = zone
        .arenas
        .get(a)
        .map_or(-3000, |x| x.cfg.get_int("Soccer", "NewGameDelay", -3000));
    let delay = if delay < 0 {
        zone.res
            .get_or_default::<Prng>()
            .below(delay.unsigned_abs() + 1)
    } else {
        delay as u32
    };
    let when = zone.now().wire().wrapping_add(delay);
    for bid in 0..n {
        phase_ball(zone, a, bid);
        if let Some(b) = data(zone, a).and_then(|d| d.balls.get_mut(bid)) {
            b.state = BallState::Waiting;
            b.carrier = None;
            b.time = when;
        }
    }
}

fn load_settings(zone: &mut Zone, a: ArenaId, spawn_balls: bool) {
    let Some(cfg) = zone.arenas.get(a).map(|x| x.cfg.clone()) else {
        return;
    };
    let count = usize::try_from(cfg.get_int("Soccer", "BallCount", 0))
        .unwrap_or(0)
        .min(MAX_BALLS);
    let mut spawn = [(0, 0, 0); 4];
    spawn[0] = (
        cfg.get_int("Soccer", "SpawnX", 512),
        cfg.get_int("Soccer", "SpawnY", 512),
        cfg.get_int("Soccer", "SpawnRadius", 20),
    );
    for (i, s) in spawn.iter_mut().enumerate().skip(1) {
        *s = (
            cfg.get_int("Soccer", &format!("SpawnX{i}"), 0),
            cfg.get_int("Soccer", &format!("SpawnY{i}"), 0),
            cfg.get_int("Soccer", &format!("SpawnRadius{i}"), 0),
        );
    }
    let now = zone.now().0;
    let Some(arena) = zone.arenas.get_mut(a) else {
        return;
    };
    let d = arena.ext.get_or_default::<ArenaBalls>();
    if count > 0 {
        d.spawn = spawn;
        d.send_time = u64::try_from(cfg.get_int("Soccer", "SendTime", 1000)).unwrap_or(1000);
        d.goal_delay = u64::try_from(cfg.get_int("Soccer", "GoalDelay", 0)).unwrap_or(0);
        d.death_goal = cfg.get_int("Soccer", "AllowGoalByDeath", 0) != 0;
    }
    if spawn_balls && count > 0 {
        d.last_sent = now;
        d.balls = vec![Ball::default(); count];
        for bid in 0..count {
            spawn_ball(zone, a, bid);
        }
        tracing::info!(target: "balls", arena = a.0, count, "arena has balls");
    } else if data(zone, a).is_some_and(|d| d.balls.len() != count) {
        set_ball_count(zone, a, count);
    }
}

/// Drops `p`'s ball where they are (ASSS `CleanupAfter`).
fn cleanup_after(zone: &mut Zone, a: ArenaId, p: PlayerId, neut: bool) {
    let Some(n) = data(zone, a).map(|d| d.balls.len()) else {
        return;
    };
    let (px, py) = zone
        .players
        .get(p)
        .map_or((0, 0), |pl| (pl.pos.x, pl.pos.y));
    let now = zone.now().wire();
    for bid in 0..n {
        let Some(b) = data(zone, a).and_then(|d| d.balls.get_mut(bid)) else {
            continue;
        };
        if b.state == BallState::Carried && b.carrier == Some(p) {
            b.state = BallState::OnMap;
            b.x = px;
            b.y = py;
            b.x_speed = 0;
            b.y_speed = 0;
            if neut {
                b.carrier = None;
            }
            b.time = now;
            send_ball(zone, a, bid);
            zone.fire(&BallFire {
                arena: a,
                player: p,
                ball: bid,
            });
        } else if neut && b.carrier == Some(p) {
            b.carrier = None;
            send_ball(zone, a, bid);
        }
    }
}

fn playing_ship(zone: &Zone, p: PlayerId) -> Option<ArenaId> {
    let pl = zone.players.get(p)?;
    if pl.status != PlayerStatus::Playing || pl.ship.is_spec() {
        return None;
    }
    pl.arena
}

fn on_pickup(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if pkt.len() != 6 {
        tracing::warn!(target: "balls", pid = p.0, len = pkt.len(), "bad ball pickup packet length");
        return;
    }
    let Ok(req) = c2s::PickupBall::decode(pkt) else {
        return;
    };
    let Some(a) = playing_ship(zone, p) else {
        return;
    };
    let bid = usize::from(req.ball_id);
    let (px, py, freq) = zone
        .players
        .get(p)
        .map_or((0, 0, 0), |pl| (pl.pos.x, pl.pos.y, pl.freq as i16));
    let Some(d) = data(zone, a) else { return };
    let Some(b) = d.balls.get(bid).copied() else {
        tracing::warn!(target: "balls", pid = p.0, "tried to pick up a nonexistent ball");
        return;
    };
    if b.state != BallState::OnMap || req.time != b.time {
        tracing::debug!(target: "balls", pid = p.0, bid, "ball pickup lost a race or used stale coordinates");
        return;
    }
    if d.balls
        .iter()
        .any(|o| o.state == BallState::Carried && o.carrier == Some(p))
    {
        return;
    }
    d.balls[bid] = Ball {
        state: BallState::Carried,
        x: px,
        y: py,
        x_speed: 0,
        y_speed: 0,
        carrier: Some(p),
        freq,
        time: 0,
    };
    send_ball(zone, a, bid);
    zone.fire(&BallPickup {
        arena: a,
        player: p,
        ball: bid,
    });
}

fn on_fire(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if pkt.len() != 16 {
        tracing::warn!(target: "balls", pid = p.0, len = pkt.len(), "bad ball fire packet length");
        return;
    }
    let Ok(f) = c2s::ShootBall::decode(pkt) else {
        return;
    };
    let Some(a) = playing_ship(zone, p) else {
        return;
    };
    let bid = usize::from(f.ball_id);
    let freq = zone.players.get(p).map_or(0, |pl| pl.freq as i16);
    let Some(b) = data(zone, a).and_then(|d| d.balls.get_mut(bid)) else {
        return;
    };
    if b.state != BallState::Carried || b.carrier != Some(p) {
        tracing::warn!(target: "balls", pid = p.0, bid, "tried to fire a ball it wasn't carrying");
        return;
    }
    *b = Ball {
        state: BallState::OnMap,
        x: f.x,
        y: f.y,
        x_speed: f.x_speed,
        y_speed: f.y_speed,
        carrier: Some(p),
        freq,
        time: f.time,
    };
    send_ball(zone, a, bid);
    zone.fire(&BallFire {
        arena: a,
        player: p,
        ball: bid,
    });
}

fn on_goal(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    if pkt.len() != 6 {
        tracing::warn!(target: "balls", pid = p.0, len = pkt.len(), "bad goal packet length");
        return;
    }
    let Ok(g) = c2s::Goal::decode(pkt) else {
        return;
    };
    let Some(pl) = zone.players.get(p) else {
        return;
    };
    let (Some(a), PlayerStatus::Playing) = (pl.arena, pl.status) else {
        return;
    };
    let bid = usize::from(g.ball_id);
    let Some((b, delay)) =
        data(zone, a).and_then(|d| d.balls.get(bid).copied().map(|b| (b, d.goal_delay)))
    else {
        tracing::warn!(target: "balls", pid = p.0, "goal for a nonexistent ball");
        return;
    };
    // carrier None marks an already-scored ball (duplicate goal packets)
    if b.carrier.is_none() {
        return;
    }
    if b.state != BallState::OnMap {
        tracing::warn!(target: "balls", pid = p.0, "goal for a carried ball");
        return;
    }
    if b.carrier != Some(p) {
        tracing::warn!(target: "balls", pid = p.0, "goal for a ball it didn't fire");
        return;
    }
    zone.fire(&Goal {
        arena: a,
        player: p,
        ball: bid,
        x: g.x,
        y: g.y,
    });
    let still_on_map = data(zone, a)
        .and_then(|d| d.balls.get(bid))
        .is_some_and(|b| b.state == BallState::OnMap);
    if !still_on_map {
        return;
    }
    if delay == 0 {
        spawn_ball(zone, a, bid);
    } else {
        phase_ball(zone, a, bid);
        let when = zone.now().wire().wrapping_add(delay as u32);
        if let Some(b) = data(zone, a).and_then(|d| d.balls.get_mut(bid)) {
            b.state = BallState::Waiting;
            b.carrier = None;
            b.time = when;
        }
    }
}

fn ball_timer(zone: &mut Zone) -> bool {
    let now = zone.now();
    for a in zone.arenas.ids() {
        if zone
            .arenas
            .get(a)
            .is_none_or(|x| x.status != ArenaStatus::Running)
        {
            continue;
        }
        let Some(d) = data(zone, a) else { continue };
        if d.balls.is_empty() || now.0.saturating_sub(d.last_sent) <= d.send_time {
            continue;
        }
        d.last_sent = now.0;
        for bid in 0..d.balls.len() {
            let Some(b) = data(zone, a).and_then(|d| d.balls.get(bid).copied()) else {
                continue;
            };
            match b.state {
                BallState::OnMap => send_ball(zone, a, bid),
                BallState::Carried => {
                    let Some(c) = b.carrier else { continue };
                    let pos = zone.players.get(c).map(|pl| (pl.pos.x, pl.pos.y));
                    if let (Some((x, y)), Some(bm)) =
                        (pos, data(zone, a).and_then(|d| d.balls.get_mut(bid)))
                    {
                        bm.x = x;
                        bm.y = y;
                    }
                    send_ball(zone, a, bid);
                }
                BallState::Waiting => {
                    if (now.wire().wrapping_sub(b.time) as i32) > 0 {
                        spawn_ball(zone, a, bid);
                    }
                }
                BallState::None => {}
            }
        }
    }
    true
}

/// The ball interface (ASSS `Iballs`).
pub trait Balls: Send + Sync {
    /// Changes the number of balls.
    fn set_ball_count(&self, zone: &mut Zone, arena: ArenaId, count: usize);
    /// Puts a ball somewhere.
    fn place_ball(&self, zone: &mut Zone, arena: ArenaId, bid: usize, ball: Ball);
    /// Ends the soccer game.
    fn end_game(&self, zone: &mut Zone, arena: ArenaId);
    /// The arena's balls.
    fn balls(&self, zone: &Zone, arena: ArenaId) -> Vec<Ball>;
}

/// The ball implementation.
pub struct BallsImpl;

impl Balls for BallsImpl {
    fn set_ball_count(&self, zone: &mut Zone, arena: ArenaId, count: usize) {
        set_ball_count(zone, arena, count);
    }
    fn place_ball(&self, zone: &mut Zone, arena: ArenaId, bid: usize, ball: Ball) {
        place_ball(zone, arena, bid, ball);
    }
    fn end_game(&self, zone: &mut Zone, arena: ArenaId) {
        end_game(zone, arena);
    }
    fn balls(&self, zone: &Zone, arena: ArenaId) -> Vec<Ball> {
        balls(zone, arena)
    }
}

/// The `balls` module.
pub struct BallsModule;

impl Module for BallsModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn Balls>(me, Scope::Global, Arc::new(BallsImpl));
        zone.on_packet(me, c2s::kind::PICKUP_BALL, on_pickup);
        zone.on_packet(me, c2s::kind::SHOOT_BALL, on_fire);
        zone.on_packet(me, c2s::kind::GOAL, on_goal);
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.insert(ArenaBalls::default());
                }
                load_settings(z, ev.arena, true);
            }
            ArenaActionKind::ConfChanged => load_settings(z, ev.arena, false),
            ArenaActionKind::Destroy => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.remove::<ArenaBalls>();
                }
            }
            _ => {}
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if let (Action::LeaveArena, Some(a)) = (ev.action, ev.arena) {
                cleanup_after(z, a, ev.player, true);
            }
        });
        zone.on::<ShipFreqChange>(me, Scope::Global, |z, ev| {
            cleanup_after(z, ev.arena, ev.player, true)
        });
        zone.on::<Kill>(me, Scope::Global, |z, k| {
            let death_goal = data(z, k.arena).is_some_and(|d| d.death_goal);
            cleanup_after(z, k.arena, k.killed, !death_goal);
        });
        zone.set_timer(me, 300, 50, ball_timer);
        Ok(())
    }
}
