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

With no arguments the server runs its bundled default zone (the ASSS 1.4.4
sample zone, compiled into the binary; see `zone/README.md`) on UDP port 5000.

```
oss-server --zone <dir>        # run an ASSS-style zone directory unchanged
oss-server --init-zone <dir>   # write the default zone to disk to customise it
oss-server --bind 0.0.0.0:6000 --log "info,net=debug" --json
```

### Local user database (biller)

`oss-biller` is a small SSC-compatible billing server backed by SQLite, for
private and test zones: accounts, play time, banners, scores, `?chat`
channels, squads and messages between zones.

```
cargo run --release -p oss-biller -- --bind 127.0.0.1:1850 --db biller.db --password <pw>
```

Point the zone at it in `conf/global.conf` (`[Billing] IP`, `Port`,
`Password`, `ServerID`, `ServerName`) and load `billing_ssc` in
`conf/modules.conf`.

## Licence

GPL-2.0-or-later (see `LICENSE`), matching ASSS, whose behaviour this server follows.

SubSpace and Continuum are the names of the original games; this project is an independent, compatible implementation and is not affiliated with their authors.
