//! Open SubSpace zone server library.
//!
//! The binary in `main.rs` assembles the default module set; private or custom
//! builds can link this library and register additional modules (ciphers,
//! security, billing) without patching it. See `docs/architecture.md`.

pub mod config;
