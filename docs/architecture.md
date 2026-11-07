# oss-server architecture

`oss-server` is a library plus a thin binary. Everything the server does is a
**module** registered with the library at startup, so a private or third-party
build can link the library, add its own modules (a cipher, a security checker,
a billing client) and run, without patching this repository.

The design follows ASSS (whose behaviour we reproduce) but is expressed in Rust
terms: no global mutable state, no locks in game logic, typed interfaces.

## Threads and data flow

```
 UDP game port ── recv task ─┐                       ┌─ UdpIo::send_to (non-blocking)
                             ├─► Inbound queue ─► game thread ─┤
 ZoneHandle::run(closure) ───┘    (bounded)            │       └─ Transport (oss-net core protocol)
                                                        ▼
 UDP ping port ── ping task ◄── PingSnapshot ◄──── Zone (players, arenas, modules, timers)
```

* **Game thread** (`runtime::GameLoop`): owns the `Zone` and the `Transport`.
  It sleeps until a datagram/closure arrives or the next 10 ms tick is due.
  Input is handled immediately (no tick quantisation of latency); timers and
  transport housekeeping run at SubSpace's native 100 Hz. Queued output is
  flushed after every input batch and every tick.
* **I/O tasks** (tokio): receive datagrams into the bounded inbound queue
  (overflow is dropped, like UDP loss) and answer zone pings from a snapshot
  the game loop refreshes every `Net:PingDataRefreshTime` ticks.
* **Other async work** (billing, database, admin HTTP API) runs on tokio and
  hands results to game logic with `ZoneHandle::run(|zone| ...)`, the
  equivalent of ASSS's `RunInMain`. The handle is available to modules as the
  `runtime::ZoneHandle` zone resource.

Graceful shutdown (Ctrl-C, SIGTERM, or a module calling
`zone.request_shutdown()`): the loop raises `ZoneStopping`, lets the transport
say goodbye to clients, unloads modules newest-first and flushes.

## The zone

`zone::Zone` is the single context passed as `&mut Zone` to every handler:

| field | what |
|---|---|
| `cfg` | `config::ConfigManager`: `global.conf`, arena configs, other `.conf` files |
| `players` / `arenas` | core tables (`player::Player`, `arena::Arena`) |
| `reg` | `registry::Registry`: interfaces, event handlers, packet handlers |
| `commands` | the `?command` table |
| `timers` | tick timers |
| `res` | module state, one value per type (`zone.res.get_mut::<MyState>()`) |
| `modules` | loaded modules |

Players and arenas carry `ext`, a typed map for per-player/per-arena module
data (ASSS `AllocatePlayerData`/`AllocateArenaData`).

Because all game logic runs on one thread with `&mut Zone`, re-entrancy is
plain function calls: module A's handler may call module B's interface, which
may raise an event that module C handles. The only rule is the usual Rust one:
don't hold a borrow of something inside the zone (a `&mut Player`, a resource)
across a call that takes `&mut Zone`. Copy out what you need, call, then look
it up again.

## Modules

```rust
pub trait Module: Send + Sync + 'static {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError>;
    fn post_load(&self, zone: &mut Zone, me: Owner) {}
    fn unload(&self, zone: &mut Zone, me: Owner) {}
    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError>; // optional
    fn detach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) {}
}
```

* A module keeps its state in the zone (`zone.res`, `player.ext`,
  `arena.ext`), not in `self`; the module value is shared and its methods take
  `&self`.
* In `load` it registers what it offers. Every registration is tagged with its
  `Owner`, and the manager removes all of it (interfaces, handlers, commands,
  timers) on unload, so `unload` is usually empty.
* `post_load` runs after the whole initial set is loaded: look up optional
  interfaces of other modules there.
* `attach`/`detach` make a module usable per arena (`[Modules] AttachModules`
  in `arena.conf`); arena-scoped registrations are removed on detach.

Modules are listed in a `module::ModuleCatalog` under a canonical name and
aliases. The aliases include ASSS names, so a zone's `conf/modules.conf` is read
unchanged (through the config preprocessor, as ASSS does). ASSS modules whose
work the core does itself (`mainloop`, `net`, `config`, `logman`, ...) are
accepted as no-ops; unknown names are logged and skipped.

### Interfaces

An interface is a trait. A module provides an implementation and others look it
up by the trait type:

```rust
pub trait Chat: Send + Sync {
    fn send_arena_message(&self, zone: &mut Zone, arena: ArenaId, text: &str);
}

// provider, in load():
zone.reg.provide::<dyn Chat>(me, Scope::Global, Arc::new(ChatImpl));

// user:
if let Some(chat) = zone.iface::<dyn Chat>() {
    chat.send_arena_message(zone, arena, "hello");
}
```

Interface objects are stateless handles (state lives in the zone), so calling
one while passing `&mut Zone` is fine. An implementation registered with
`Scope::Arena(id)` shadows the global one for that arena (`zone.iface_in`).

### Events

An event is a struct implementing `registry::Event`. `zone.fire(&event)` runs
global handlers, then handlers registered for the event's arena (if
`Event::arena` returns one), in registration order. Handler lists are
copy-on-write snapshots, so handlers can register or remove handlers while an
event is being dispatched.

```rust
pub struct PlayerEntered { pub player: PlayerId, pub arena: ArenaId }
impl Event for PlayerEntered {
    fn arena(&self) -> Option<ArenaId> { Some(self.arena) }
}

zone.on::<PlayerEntered>(me, Scope::Global, |zone, ev| { /* ... */ });
```

### Packets, commands, timers

* `zone.on_packet(me, 0x06, |zone, from, bytes| ...)`: game packets by type
  byte. The transport decodes the core protocol and calls
  `zone.handle_packet(player, bytes)`.
* `zone.send(player, bytes, SendFlags::RELIABLE)`, `zone.send_to_arena(...)`:
  packets go into the zone's outbox, which stores each packet's bytes once even
  when it is sent to many players; the transport drains it.
* `zone.add_command(me, "name", arena_or_none, help, |zone, inv| ...)`.
* `zone.set_timer(me, delay_ticks, interval_ticks, |zone| keep_running)`.

## Transport and security hooks

`net::Transport` turns datagrams into game packets and back. The default
transport is built on `oss-net` (sans-IO SubSpace core protocol: reliable
channel, clustering, big/sized transfers, VIE encryption). Tests use
`net::Loopback`, which skips the network entirely: inject packets with
`zone.handle_packet` and read what was sent from the loopback.

The Continuum encryption and security checks are not part of this repository.
A private build registers them as ordinary modules: the cipher via the
encryption interface the transport consults when a client connects, and the
security checks as a module handling the security packets (it can use
`clientset::SettingsBlock::checksum` for the settings checksum). No code here
needs to change for that.

## Config

`config` reproduces ASSS 1.4.4 exactly: the preprocessor (`#include`,
`#define`, `#ifdef`/`#ifndef`/`#else`/`#endif`, `\` continuations, `;`/`/`
comments), `[Section]` and `Section:Key` syntax, escapes, case-insensitive keys
where the last definition wins, `strtol`-style integers, the config search path
(`arenas/<base>/<name>`, `conf/<name>`, `<name>`, `arenas/(default)/<name>`),
and sharing of parsed files by resolved path. Files are decoded as Latin-1 so
every byte survives.

`clientset` builds the 1428-byte settings packet from an arena's config the way
ASSS's `clientset.c` does, and implements ASSS override keys, the settings
checksum and weighted random prizes on the same bytes. It is checked against
output generated by ASSS's own code (`tools/clientset-oracle`).

## Adding a module: checklist

1. Write a type implementing `Module`; keep state in a resource or `ext`.
2. Register interfaces/handlers/commands/timers in `load` with your `Owner`.
3. Add it to `modules::builtin_catalog()` with its ASSS name as an alias, or,
   in a private build, call `ServerBuilder::module(name, aliases, factory)`.
4. Test it against a `Zone` built from a `config::MemSource` with
   `net::Loopback` as the transport; no sockets needed.
5. Untrusted input (packets, client-supplied names) must never panic: decode
   with checked reads and drop bad packets.
