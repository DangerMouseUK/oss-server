# Contributing to oss-server

Thanks for looking. This file is the short version of what a change needs to be
mergeable here, so that you find out before you spend an evening on it rather
than after.

## Before you commit anything

**Read [the "What must never reach this repository" section of
`CLAUDE.md`](CLAUDE.md#what-must-never-reach-this-repository).** It is not long,
it is the same in all four of the project's public repositories, and it applies
to commit messages, branch names and pull-request text as well as to files.

Two things about it that are easy to miss: it is a **test**, not just a list —
the list is worked examples — and the local guard that enforces part of it is
**not present in a fresh clone**. If you are working from your own clone, the
written rule and review are the only checks there are. If you think something
you want to write might be covered, ask in an issue first. Nobody will mind.

## ASSS, and what you may bring in

This repository is **GPL-2.0-or-later**, so there is no licence barrier between
it and ASSS, which is also GPL. Three things are true at once, and it is worth
having all three rather than one:

1. **This project has never used ASSS source, in any repository.** The server
   was written fresh in Rust, from the protocol and from observed behaviour.
2. **A contribution carrying ASSS code is not forbidden here and will not be
   refused on principle.**
3. **We do not intend to stand in the way of what you want to attempt.** This
   project is fully open source, and that includes not inventing rules to stop
   you working the way you prefer.

**On the apparent contradiction with the README.** [The README's licence
section](README.md#licence) says plainly that no ASSS source is in this
repository. That is a **description of the code that is here**, not a
prohibition on what may arrive. This page rules on what may arrive, and the
answer is that it may. The two are not in conflict; they are answering different
questions.

**What we would honestly tell you, though,** is that porting is unlikely to be a
good way to spend your time — and this is a practical observation, not a rule
dressed up as one. ASSS is C. Turning C into idiomatic, safe Rust that satisfies
this repository's invariants — untrusted input never panicking, no `unwrap` on
anything off the wire, the performance budget, `cargo deny` passing, the four
gates below — is usually **more** work than implementing the behaviour from the
protocol in the first place, and it tends to produce code that reads like
transliterated C and is harder to review. Every part of this server so far was
built the other way, and that was not for licence reasons.

So: if you want to port something, port it. We will review it on its merits like
anything else. **Just tell us in the pull request that that is what you did**,
and name what it came from in the file headers. The project already tracks
provenance this way for recorded test data, so there is a convention to follow
rather than a new ceremony to invent, and the GPL expects attribution in any
case.

**A note on the other repository:** [oss-core](https://github.com/DangerMouseUK/oss-core)
is **MIT**, and GPL code must not go into it at all. That rule is real and it is
the opposite of this one. Do not carry this page's answer across — read
[oss-core's `CONTRIBUTING.md`](https://github.com/DangerMouseUK/oss-core/blob/main/CONTRIBUTING.md).

## The invariants

These are what review will actually check:

- **Untrusted input never panics the server.** No `unwrap` or `expect` on
  network or file data. A panic reachable from the wire is a remote crash.
- **The performance budget** is 500 players in one arena at under 20% of one
  core. Avoid per-packet allocations on hot paths, and measure with benches
  before optimising rather than after.
- **Pluggable concerns stay pluggable.** This is a library with a thin binary,
  and private or third-party builds link the library and register their own
  modules. Anything swappable is a trait registered at startup, never a
  hard-coded match.
- **Config compatibility is exact.** The ASSS `.conf` format is read unchanged,
  and the client settings packet must be byte-identical to what ASSS builds from
  the same `arena.conf`.
- Tests use synthetic zones and maps, or small hand-made fixtures. Do not commit
  third-party binaries, zone downloads, captures, personal data or machine
  paths.

## Before you open a pull request

Run all four. CI runs the same ones, so this only saves you a round trip:

```sh
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

**`cargo fmt`, not `cargo fmt --all`.** This one matters and it is not a style
preference: `--all` takes its package list from `cargo metadata`, which includes
path dependencies outside this workspace. In this repository that hands rustfmt
every source root in a sibling `oss-core` checkout and **rewrites it in place**,
across a repository boundary, without saying so. Plain `cargo fmt` covers this
repository's own crates, which is what is wanted.

**The fourth gate is not optional and the other three do not stand in for it.**
It is the only one that catches a rustdoc fault — a public item linking to a
private one, a broken intra-doc link, a bad code fence — and `main` has gone red
on exactly that after the other three passed locally.

## Changes that span repositories

Protocol, transport and format code lives in
[oss-core](https://github.com/DangerMouseUK/oss-core) and is shared with the
client. **Do not fork it into this repository**; request the change there. If
your server-side fix really needs a different packet decoder, that is an
`oss-core` change, and it lands there first because this repository builds
against it.

## Practical things

- **Open an issue first for anything large.** For a typo or an obvious fix, just
  send the pull request.
- Commits: imperative subject, small and focused.
- There is no CLA and no copyright assignment. Your contribution is yours, under
  this repository's licence.
- One maintainer, volunteering. Reviews may be slow. Pinging a quiet pull
  request after a week or so is welcome, not rude.

## Security

**Do not report a security vulnerability in a pull request or a public issue.**
See [`SECURITY.md`](SECURITY.md) — there is a private channel, and it is the
right way in. That matters more here than in most repositories: this is a
network server, and anything you found is probably reachable by a stranger on
somebody else's zone too.
