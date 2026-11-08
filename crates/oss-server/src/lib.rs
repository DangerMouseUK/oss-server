//! Open SubSpace zone server library.
//!
//! The binary in `main.rs` assembles the default module set; private or custom
//! builds can link this library and register additional modules (ciphers,
//! security, billing) without patching it. See `docs/architecture.md`.
//!
//! * [`zone::Zone`] holds all game state and is passed to every module.
//! * [`module`] defines modules and loads them (from `conf/modules.conf`).
//! * [`registry`] holds typed interfaces, events and packet handlers.
//! * [`runtime`] runs the game thread and the network I/O tasks.
//! * [`config`] reads ASSS config files unchanged; [`clientset`] builds the
//!   settings packet from them.

pub mod admin;
pub mod arena;
pub mod clientset;
pub mod commands;
pub mod config;
pub mod default_zone;
pub mod logging;
pub mod metrics;
pub mod module;
pub mod modules;
pub mod net;
pub mod ping;
pub mod player;
pub mod prng;
pub mod registry;
pub mod runtime;
pub mod time;
pub mod timers;
pub mod transport;
pub mod zone;
