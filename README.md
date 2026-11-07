# oss-server

An open-source zone server for SubSpace/Continuum, written in Rust. Goals:

- Accept Continuum, VIE and Open SubSpace clients on one UDP port.
- Run unchanged from an existing ASSS-style zone directory (`*.conf`, `*.lvl`, `*.lvz`).
- Server-authoritative validation so open clients are safe to allow.
- Modern operations: structured logs, metrics, an admin API, one-command deploys.

Status: early development.

## Building

`oss-server` uses the shared crates in [oss-core](https://github.com/DangerMouseUK/oss-core) as a sibling checkout:

```
git clone https://github.com/DangerMouseUK/oss-core
git clone https://github.com/DangerMouseUK/oss-server
cd oss-server
cargo run --release
```

## Licence

GPL-2.0-or-later (see `LICENSE`), matching ASSS, whose behaviour this server follows.

SubSpace and Continuum are the names of the original games; this project is an independent, compatible implementation and is not affiliated with their authors.
