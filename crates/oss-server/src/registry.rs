//! Typed interfaces, events and packet handlers: the ASSS module "glue"
//! (`GetInterface`, `RegCallback`/`DO_CBS`, `AddPacket`) in idiomatic Rust.
//!
//! * **Interfaces** are trait objects looked up by their trait type:
//!   `zone.iface::<dyn Chat>()` returns an `Arc<dyn Chat>`. A module provides
//!   one with [`Registry::provide`]. An arena may carry its own implementation
//!   that shadows the global one (ASSS's per-arena interfaces).
//! * **Events** are plain structs implementing [`Event`]. Handlers registered
//!   globally see every event; handlers registered for an arena see only events
//!   whose [`Event::arena`] is that arena (ASSS callbacks registered "for an
//!   arena" by attached modules).
//! * **Packet handlers** are keyed by the game packet type byte.
//!
//! Handler lists are `Arc<[...]>` snapshots: dispatch clones the `Arc` (no
//! allocation), so handlers may register or remove handlers while running
//! without invalidating the iteration.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use crate::arena::ArenaId;
use crate::player::PlayerId;
use crate::zone::Zone;

/// Identifies the module that registered something, so it can all be removed
/// when the module is unloaded or detached from an arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner(pub u32);

impl Owner {
    /// The zone core itself.
    pub const CORE: Owner = Owner(0);
}

/// Where a handler applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// All arenas and zone-wide events.
    Global,
    /// Only events in this arena.
    Arena(ArenaId),
}

/// An event modules can raise and observe. `arena` routes it to handlers
/// registered for that arena in addition to global ones.
pub trait Event: Any {
    /// The arena this event happened in, if any.
    fn arena(&self) -> Option<ArenaId> {
        None
    }
}

/// A handler for events of type `E`.
pub type Handler<E> = Arc<dyn Fn(&mut Zone, &E) + Send + Sync>;

/// A handler for a game packet: `(zone, sender, packet bytes including type)`.
pub type PacketHandler = Arc<dyn Fn(&mut Zone, PlayerId, &[u8]) + Send + Sync>;

pub(crate) struct Entry<H: ?Sized> {
    owner: Owner,
    handler: Arc<H>,
}

impl<H: ?Sized> Clone for Entry<H> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner,
            handler: self.handler.clone(),
        }
    }
}

/// Copy-on-write handler list.
struct List<H: ?Sized> {
    items: Arc<[Entry<H>]>,
}

impl<H: ?Sized> Default for List<H> {
    fn default() -> Self {
        Self {
            items: Arc::from(Vec::new()),
        }
    }
}

impl<H: ?Sized> List<H> {
    fn push(&mut self, owner: Owner, handler: Arc<H>) {
        let mut v: Vec<Entry<H>> = self.items.to_vec();
        v.push(Entry { owner, handler });
        self.items = v.into();
    }

    fn remove_owner(&mut self, owner: Owner) {
        if self.items.iter().any(|e| e.owner == owner) {
            let v: Vec<Entry<H>> = self
                .items
                .iter()
                .filter(|e| e.owner != owner)
                .cloned()
                .collect();
            self.items = v.into();
        }
    }
}

type ErasedList = Box<dyn Any + Send + Sync>;
type IfaceKey = (TypeId, Option<ArenaId>);
type EventKey = (TypeId, Scope);
type Remover = fn(&mut ErasedList, Owner);
type Snapshot<H> = Option<Arc<[Entry<H>]>>;

/// Interfaces, event handlers and packet handlers registered by modules.
pub struct Registry {
    ifaces: HashMap<IfaceKey, (Owner, ErasedList)>,
    events: HashMap<EventKey, ErasedList>,
    event_removers: HashMap<EventKey, Remover>,
    packets: Vec<List<PacketFn>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            ifaces: HashMap::new(),
            events: HashMap::new(),
            event_removers: HashMap::new(),
            packets: (0..256).map(|_| List::default()).collect(),
        }
    }
}

fn remove_owner_erased<E: Event>(list: &mut ErasedList, owner: Owner) {
    if let Some(l) = list.downcast_mut::<List<dyn Fn(&mut Zone, &E) + Send + Sync>>() {
        l.remove_owner(owner);
    }
}

impl Registry {
    /// Provides an implementation of interface `I` (usually `dyn SomeTrait`),
    /// globally or for one arena. Replaces any previous provider in that scope.
    pub fn provide<I: ?Sized + Send + Sync + 'static>(
        &mut self,
        owner: Owner,
        scope: Scope,
        iface: Arc<I>,
    ) {
        let arena = match scope {
            Scope::Global => None,
            Scope::Arena(a) => Some(a),
        };
        self.ifaces
            .insert((TypeId::of::<I>(), arena), (owner, Box::new(iface)));
    }

    /// Looks up interface `I`, preferring the arena-specific provider.
    pub fn get<I: ?Sized + Send + Sync + 'static>(&self, arena: Option<ArenaId>) -> Option<Arc<I>> {
        let find = |a: Option<ArenaId>| {
            self.ifaces
                .get(&(TypeId::of::<I>(), a))
                .and_then(|(_, b)| b.downcast_ref::<Arc<I>>())
        };
        arena
            .and_then(|a| find(Some(a)))
            .or_else(|| find(None))
            .cloned()
    }

    /// Registers an event handler.
    pub fn on<E: Event>(&mut self, owner: Owner, scope: Scope, handler: Handler<E>) {
        let key = (TypeId::of::<E>(), scope);
        self.event_removers.insert(key, remove_owner_erased::<E>);
        let list = self
            .events
            .entry(key)
            .or_insert_with(|| Box::new(List::<dyn Fn(&mut Zone, &E) + Send + Sync>::default()));
        if let Some(l) = list.downcast_mut::<List<dyn Fn(&mut Zone, &E) + Send + Sync>>() {
            l.push(owner, handler);
        }
    }

    /// Snapshot of the handlers that should see `event`: global first, then
    /// the event's arena.
    pub(crate) fn handlers<E: Event>(&self, event: &E) -> [Snapshot<HandlerFn<E>>; 2] {
        let get = |scope| {
            self.events
                .get(&(TypeId::of::<E>(), scope))
                .and_then(|l| l.downcast_ref::<List<HandlerFn<E>>>())
                .map(|l| l.items.clone())
        };
        [
            get(Scope::Global),
            event.arena().and_then(|a| get(Scope::Arena(a))),
        ]
    }

    /// Registers a handler for game packets of type `kind`.
    pub fn on_packet(&mut self, owner: Owner, kind: u8, handler: PacketHandler) {
        self.packets[usize::from(kind)].push(owner, handler);
    }

    pub(crate) fn packet_handlers(&self, kind: u8) -> Arc<[Entry<PacketFn>]> {
        self.packets[usize::from(kind)].items.clone()
    }

    /// Removes everything `owner` registered, globally and in every arena.
    pub fn remove_owner(&mut self, owner: Owner) {
        self.ifaces.retain(|_, (o, _)| *o != owner);
        for (key, list) in &mut self.events {
            if let Some(rm) = self.event_removers.get(key) {
                rm(list, owner);
            }
        }
        for l in &mut self.packets {
            l.remove_owner(owner);
        }
    }

    /// Removes what `owner` registered for one arena (module detach).
    pub fn remove_owner_in_arena(&mut self, owner: Owner, arena: ArenaId) {
        self.ifaces
            .retain(|(_, a), (o, _)| !(*o == owner && *a == Some(arena)));
        for (key, list) in &mut self.events {
            if key.1 == Scope::Arena(arena)
                && let Some(rm) = self.event_removers.get(key)
            {
                rm(list, owner);
            }
        }
    }

    /// Drops all arena-scoped registrations for a destroyed arena.
    pub fn forget_arena(&mut self, arena: ArenaId) {
        self.ifaces.retain(|(_, a), _| *a != Some(arena));
        self.events.retain(|(_, s), _| *s != Scope::Arena(arena));
        self.event_removers
            .retain(|(_, s), _| *s != Scope::Arena(arena));
    }
}

pub(crate) type HandlerFn<E> = dyn Fn(&mut Zone, &E) + Send + Sync;
pub(crate) type PacketFn = dyn Fn(&mut Zone, PlayerId, &[u8]) + Send + Sync;

impl<H: ?Sized> Entry<H> {
    pub(crate) fn handler(&self) -> &Arc<H> {
        &self.handler
    }
}

/// Typed per-zone state owned by modules (ASSS module-local globals), keyed by
/// type. Only one value of each type exists.
#[derive(Default)]
pub struct Resources {
    map: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl Resources {
    /// Inserts or replaces a resource, returning the old value.
    pub fn insert<T: Any + Send>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|b| b.downcast::<T>().ok())
            .map(|b| *b)
    }

    /// Borrows a resource.
    pub fn get<T: Any + Send>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref())
    }

    /// Mutably borrows a resource.
    pub fn get_mut<T: Any + Send>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut())
    }

    /// Mutably borrows a resource, inserting `T::default()` if absent.
    pub fn get_or_default<T: Any + Send + Default>(&mut self) -> &mut T {
        let b = self
            .map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()));
        match b.downcast_mut() {
            Some(v) => v,
            None => unreachable!("resource map keyed by TypeId"),
        }
    }

    /// Removes a resource.
    pub fn remove<T: Any + Send>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast::<T>().ok())
            .map(|b| *b)
    }
}

/// Per-player or per-arena extension data (ASSS `AllocatePlayerData`): typed
/// values attached to one object, created on first use.
pub type Extensions = Resources;
