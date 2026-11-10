//! `buy`: buying prizes with points (ASSS `buy.c`). Attach it to an arena.
//!
//! `?buy` on its own lists what this arena sells and what it costs; `?buy
//! <item>` buys one. An item costs `Cost:<name>` points and is not for sale
//! when that is 0, which is every item until a zone says otherwise.
//!
//! A buyer must be in a ship and, unless `Cost:PurchaseAnytime` is set, in a
//! safe zone. The price comes off their flag points rather than their kill
//! points, as in ASSS, so buying doesn't change anyone's kill average.

use oss_proto::s2c::PrizeReceive;

use crate::arena::ArenaId;
use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::Owner;
use crate::zone::Zone;

use super::chat::message;
use super::stats::{Stats, stat};

/// What a zone can sell: the word a player types, the `Cost:` setting, and
/// the SubSpace prize number. ASSS's table, in its order.
const ITEMS: &[(&str, &str, i16)] = &[
    ("x", "XRadar", 6),
    ("recharge", "Recharge", 1),
    ("energy", "Energy", 2),
    ("rot", "Rotation", 3),
    ("stealth", "Stealth", 4),
    ("cloak", "Cloak", 5),
    ("gun", "Gun", 8),
    ("bomb", "Bomb", 9),
    ("bounce", "Bounce", 10),
    ("thrust", "Thrust", 11),
    ("speed", "Speed", 12),
    ("multi", "MultiFire", 15),
    ("prox", "Prox", 16),
    ("super", "Super", 17),
    ("shield", "Shield", 18),
    ("shrap", "Shrap", 19),
    ("anti", "AntiWarp", 20),
    ("rep", "Repel", 21),
    ("burst", "Burst", 22),
    ("decoy", "Decoy", 23),
    ("thor", "Thor", 24),
    ("brick", "Brick", 26),
    ("rocket", "Rocket", 27),
    ("port", "Portal", 28),
];

/// ASSS's safe-zone status bit.
const SAFE_ZONE: u8 = 0x20;

fn cost_of(zone: &Zone, arena: ArenaId, setting: &str) -> i32 {
    zone.arenas
        .get(arena)
        .map_or(0, |a| a.cfg.get_int("Cost", setting, 0))
}

fn print_costs(zone: &mut Zone, p: PlayerId, arena: ArenaId) {
    let mut any = false;
    for (_, setting, _) in ITEMS {
        let cost = cost_of(zone, arena, setting);
        if cost != 0 {
            message(zone, p, &format!("buy: {setting:<9} {cost:6}"));
            any = true;
        }
    }
    if !any {
        message(
            zone,
            p,
            "There are no items available to purchase in this arena.",
        );
    }
}

fn c_buy(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let me = inv.sender;
    // Not trimmed: ASSS tests `!*params`, so an argument of nothing but
    // whitespace is not "no argument" -- it goes to the item lookup and is
    // refused there as an unknown item.
    let params = inv.params;
    if params.is_empty() {
        print_costs(zone, me, arena);
        return;
    }
    // ASSS runs strstr over the table in table order with no break, so the
    // LAST entry whose string occurs anywhere in the arguments wins -- not
    // the first, and not the best. "thorax" buys Thor, "exit" buys XRadar,
    // "report" buys Portal, and the test is case-sensitive, so "Bomb" buys
    // nothing at all.
    let item = ITEMS.iter().rfind(|(word, ..)| params.contains(word));
    let Some(&(_, setting, prize)) = item else {
        message(zone, me, "Invalid item specified for purchase.");
        return;
    };
    let cost = cost_of(zone, arena, setting);
    if cost == 0 {
        message(zone, me, "That item isn't available for purchase.");
        return;
    }
    let Some(player) = zone.players.get(me) else {
        return;
    };
    if player.ship.is_spec() {
        message(zone, me, "Spectators cannot purchase items.");
        return;
    }
    let in_safe = player.pos.status & SAFE_ZONE != 0;
    let anywhere = zone
        .arenas
        .get(arena)
        .is_some_and(|a| a.cfg.get_int("Cost", "PurchaseAnytime", 0) != 0);
    if !anywhere && !in_safe {
        message(zone, me, "You must be in a safe zone to purchase items.");
        return;
    }
    let Some(stats) = zone.iface::<dyn Stats>() else {
        return;
    };
    let points = stats.get(zone, me, stat::KILL_POINTS) + stats.get(zone, me, stat::FLAG_POINTS);
    if points < cost {
        message(
            zone,
            me,
            "You don't have enough points to purchase that item.",
        );
        return;
    }
    // taken off the flag points, so a player's kill average doesn't move
    stats.increment(zone, me, stat::FLAG_POINTS, -cost);
    stats.send_updates(zone, None);
    give_prize(zone, me, prize);
    message(zone, me, &format!("Bought {setting}."));
    let name = zone
        .players
        .get(me)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    tracing::debug!(target: "buy", pid = me.0, %name, "bought {setting}");
}

/// Sends a player one prize, and tells the anti-cheat what they now hold.
fn give_prize(zone: &mut Zone, p: PlayerId, prize: i16) {
    let mut buf = Vec::with_capacity(8);
    oss_proto::Packet::encode(&PrizeReceive { count: 1, prize }, &mut buf);
    zone.send(p, &buf, crate::net::SendFlags::RELIABLE);
    super::anticheat::prize_taken(zone, p, i32::from(prize));
}

/// The `buy` module (attach to arenas).
pub struct BuyModule;

impl Module for BuyModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.add_command(
            me,
            "buy",
            Some(arena),
            Some(
                "Module: buy\nTargets: none\nArgs: [<item>]\nWith no argument, lists what this arena sells. With one, buys that item: you must be in a ship, and in a safe zone unless the arena allows buying anywhere.\n",
            ),
            c_buy,
        );
        Ok(())
    }
}
