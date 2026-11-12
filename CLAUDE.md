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
- Before committing, all four: `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, and `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`.
  **The doc gate is not optional and the other three cannot stand in for it.** CI runs it as the `docs`
  job, and it is the only one that catches a rustdoc fault — a public item linking to a private one, a
  broken intra-doc link, a bad code fence. oss-core's `main` went red on exactly that on 2026-11-11,
  after fmt, clippy and tests all passed locally.
  **Not `cargo fmt --all` in this repo.** `--all` takes its package list from `cargo metadata`,
  which includes path dependencies outside this workspace. Here that hands rustfmt 31 source
  roots in the sibling `../oss-core` checkout and **rewrites oss-core in place**, across a repo
  boundary and without saying so. It is a silent no-op only while oss-core happens to be
  rustfmt-clean — it bites exactly when someone is mid-edit there. Plain `cargo fmt` at the
  workspace root covers this repo's own crates, which is what is wanted. Measured in this repo
  on 2026-11-11 with `cargo fmt --all --verbose --check`: 31 oss-core roots under `--all`, none
  under plain `cargo fmt`. Inside oss-core itself `--all` is correct and should stay.
  **And the writing form reformats every file in the workspace, including ones another
  session has uncommitted.** Harmless while one session worked at a time; not harmless now.
  On 2026-11-12 two sessions reformatted each other's in-progress files within one hour, in
  opposite directions, and neither could have known. **When you may not be alone in the
  checkout, run `cargo fmt --check` first** — it names the files it would change — and use
  the writing form only when that list is your own; verify your own files with
  `rustfmt --check --edition 2024 <file>`. Saying afterwards that someone else's file was
  dirty does not un-reformat it.
