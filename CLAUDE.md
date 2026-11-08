# oss-server — agent conventions

Rust zone server for SubSpace/Continuum. Licence **GPL-2.0-or-later**. Behavioural reference: ASSS (GPL-2+, may be ported) and Subspace Server .NET (GPLv2).

## Architecture
- Protocol, transport and formats come from the sibling `../oss-core` crates (`oss-proto`, `oss-net`, `oss-formats`). Don't fork them here; request changes there.
- `oss-server` is a **library + thin binary**. Private or third-party builds link the library and register extra modules (ciphers, security, billing) — so every pluggable concern is a trait registered at startup, never a hard-coded match.
- Runtime: tokio UDP I/O feeding a fixed-tick (100 Hz, SubSpace's timebase) game loop; `tracing` for logs; axum for the admin/metrics HTTP API.
- ASSS-style modules: arenas, players, game, chat, flags, balls, bricks, billing, directory registration, file transfer, security, anti-cheat — each behind an interface.
- Config: the ASSS `.conf` format read unchanged (`#include`, sections, overrides), and the client settings packet must be byte-identical to what ASSS builds from the same `arena.conf`.

## Rules
- Untrusted input never panics the server. No `unwrap`/`expect` on network or file data.
- Performance budget (PRD): 500 players in one arena at under 20% of one core. Avoid per-packet allocations on hot paths; measure with benches before optimising.
- Tests use synthetic zones/maps or small hand-made fixtures. The project's default content under `zone/` (the GPL ASSS sample zone: configs and `.lvl` maps) is committed on purpose. Never commit `Continuum.exe` or its DLLs, `scrty*` files, third-party zone downloads, captures, personal data or machine paths.
- Continuum's security handshake is not implemented here; expose the hooks a private module needs.
- Before committing: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
