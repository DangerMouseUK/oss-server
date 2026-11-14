# oss-server — agent conventions

Rust zone server for SubSpace/Continuum. Licence **GPL-2.0-or-later**. Behavioural reference: ASSS (GPL-2+, may be ported) and Subspace Server .NET (GPLv2).

## What must never reach this repository

**This section is identical in all four public repositories. Change it in all four or
in none.** It exists because material that must not be public has reached a public
repository three times, in three different places, and each time the rule written
afterwards was aimed at the previous instance. So read the test first and the list
second — the list is worked examples of the test, not its boundary.

### The test

**Before anything is committed, ask: does this narrow the search for someone trying to
obtain the material that is being kept back?** If it does, it does not go in — however
small it is, however incidental to what you were doing, and whatever else in the commit
is perfectly fine.

That test is the rule. The classes below are the ones already known to have gone wrong;
**a thing can fail the test without appearing in any of them**, and when it does, the
answer is still no.

### The classes

- **The method itself, and any prose detailed enough to implement a step from.**
  Paraphrase counts. What is protected is a method, and a method survives paraphrase —
  "roughly how it works" *is* the disclosure. So is a diagram, a worked example, or a
  sequence of operations given in order.
- **How the material is put together** — its internal construction, how many parts it
  has, how they relate, what feeds what. This is disclosure on its own and needs no
  values attached to it.
- **Addresses, offsets or locations inside somebody else's binary**, named as such, with
  or without a behavioural claim attached. An address is a foothold whatever routine it
  names, and the audience best placed to use one is the audience this project is handed
  to.
- **The names of private tooling, private crates and the project's private
  repositories**, and any dependency stated between them. **A name can state a
  structure, and several of ours do** — so a bare name, with nothing else said about it,
  can be the whole leak.
- **Anything that characterises who the material is being kept back for**, or the
  circumstances under which that was settled. That the material exists and is not public
  may be said; for whom may not.
- **How this project is said to have come by anything.** Both directions are out:
  asserting it and denying it are equally disclosive, because the denial names the thing
  it denies.

### Where this applies

**Not only to file content.** All of it applies equally to:

- **commit messages**, subject and body — *the absence of this line is literally how the
  second incident happened*;
- **branch names and tag names**;
- code comments, test fixtures and their headers, documentation prose, changelog
  entries;
- issue and pull-request text, release notes, CI logs and workflow output;
- and anything a note or report quotes out of any of the above.

**A repository is its history, not its tip.** Taking something out at the tip does not
unpublish it. Once a commit is pushed, the only remedies are a rewrite and the lead's
decision, so the cheap moment is before the commit, every time.

### It applies to passing remarks

**The subject of a commit does not limit the rule.** The worst instance so far arrived
as an aside about machine load, inside a commit about an unrelated networking fix. The
file content was completely clean. Nobody was being careless — nobody was looking there.

So: an aside, a parenthesis, a "while I was at it", a tool named only to explain why a
measurement was noisy. Those are where this has actually gone wrong, not in the places
anyone was guarding.

### When you find a class that is not listed here

**Add it here, and to the guard, in the same change.** Do not handle it quietly and move
on. The recurring failure in this project has not been carelessness; it has been that
each fix was scoped to the thing that had just happened, so the next one surfaced
somewhere nobody had thought to look. A class that is noticed and not written down is a
class that gets rediscovered the hard way.

### What is permitted, so this is not read as a gag

- **Compatibility is permitted. The method is not.** A public client or server may be
  fully compatible with the original. What may not happen is telling anyone how.
- **The seam is deliberate and public-safe.** The cipher trait, the security hooks and
  the documentation describing their shape are this repository's own contents and belong
  here. They are not a leak of anything; they are what makes the rest work unchanged.
- **Saying the material exists and is being kept back is allowed**, including in public
  documentation, as long as the saying carries no part of the method and describes no
  part of its construction. The public repositories already say it, and should.
- **The VIE-era stack and OpenSec are public in full**, constants, vectors and all.
  Nothing in this section narrows them, and nothing here is a reason to redact them.

### The mechanical check

A local guard refuses a commit whose staged content, message or ref names match a
private pattern list. **The list is not in this repository and must never be**: a
denylist of forbidden terms, committed to a public repository, is itself a disclosure —
the most efficient index to the problem anyone could write. It is kept outside this
repository, and the hooks that read it live in `.git/hooks`, which is neither
committed nor cloned.

Three things about it that matter more than its existence:

- **It fails closed.** If it cannot find or read its list it refuses the commit and says
  so. A check that passes when it cannot find its list is worse than no check at all.
- **It is a floor, not a ceiling.** It matches known shapes. It has nothing to say about
  a class nobody has thought of yet, which is exactly how all three incidents happened.
  **The test at the top of this section is the rule; the guard only catches the part of
  it that a regular expression can see.**
- **A clone does not have it.** Anyone working from a fresh clone, without the private
  checkout, has no guard at all — only this section and review. That is the price of not
  publishing the list, and it is the right way round: the written rule travels, the index
  does not.

Bypassing the guard with `--no-verify` is a decision to report to the lead, not a step.

### When in doubt

**It does not go in.** There is no deadline that trades against this. It is an agreement
rather than a licensing question, so it cannot be reasoned around case by case, and it
does not expire. Ask the lead instead of deciding.

The reasoning behind all of the above is recorded as decisions **`D10`, `D17` and
`D19`**. If you think one of them is wrong, that is a question for the lead rather
than an edit.

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
  which includes path dependencies outside this workspace. Here that hands rustfmt every
  source root in the sibling `../oss-core` checkout and **rewrites oss-core in place**, across
  a repo boundary and without saying so. It is a silent no-op only while oss-core happens to be
  rustfmt-clean — it bites exactly when someone is mid-edit there. Plain `cargo fmt` at the
  workspace root covers this repo's own crates, which is what is wanted. Measured in this repo
  on 2026-11-13 with `cargo fmt --all --verbose --check` (that form only reads): 81 source
  roots under `--all`, 34 of them in `../oss-core`; 47 under plain `cargo fmt`, none in
  oss-core. Those totals climb as either repo gains crates, tests, examples or benches, so
  re-count rather than trust them — the verbose output names each root on a line beginning
  `[lib (2024)]`, and no line of it begins `Formatting`. CI's own `fmt` step has run plain
  `cargo fmt --check` since `09a8341` on 2026-11-12; read `.github/workflows/ci.yml` for what
  CI does today. Inside oss-core itself `--all` is correct and should stay.
  **And the writing form reformats every file in the workspace, including ones another
  session has uncommitted.** Harmless while one session worked at a time; not harmless now.
  On 2026-11-12 two sessions reformatted each other's in-progress files within one hour, in
  opposite directions, and neither could have known. **When you may not be alone in the
  checkout, run `cargo fmt --check` first** — it names the files it would change — and use
  the writing form only when that list is your own; verify your own files with
  `rustfmt --check --edition 2024 <file>`. Saying afterwards that someone else's file was
  dirty does not un-reformat it.
