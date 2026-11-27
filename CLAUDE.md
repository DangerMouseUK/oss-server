# oss-server — agent conventions

Rust zone server for SubSpace/Continuum. Licence **GPL-2.0-or-later**. Behavioural reference: ASSS (GPL-2+, may be ported) and Subspace Server .NET (GPLv2).

## What must never reach this repository

**This section is identical in all four public repositories. Change it in all four or
in none.** It is written as a test rather than a list on purpose: a rule written as a
list of past mistakes guards only the places where mistakes have already happened.

### The test

**Before anything is committed, ask: does this narrow the search for someone trying to
obtain the material that is being kept back?** If it does, it does not go in — however
small it is, however incidental to what you were doing, and whatever else in the commit
is perfectly fine.

That test is the rule. There is deliberately no list of examples under it: **an example
of what is being kept back would describe what is being kept back**, which is the thing
the test exists to prevent.

### What fails it

**Anything specific about the withheld material fails the test** — its nature, how it
works, how it is made or organised, where it came from, where it is kept, why it is kept
back, or anything about any of those. Paraphrase counts, and so does a partial answer: a
detail is not made safe by being small, vague or incomplete, because a search is
narrowed a little at a time. A denial counts too, since a denial names the thing it
denies.

**The names of anything private fail it as well** — repositories, tools, files, paths,
notes, and any relationship stated between them. A private name is a pointer, so a bare
name with nothing else said about it can be the whole leak.

**So does a signpost.** A sentence that tells a reader where withheld detail is kept, or
to look somewhere for it, fails the test even when it names nothing. Drop the sentence;
do not reword it into a vaguer signpost.

### Where this applies

**Not only to file content.** All of it applies equally to:

- **commit messages**, subject and body;
- **branch names and tag names**;
- code comments, test fixtures and their headers, documentation prose, changelog
  entries;
- issue and pull-request text, release notes, CI logs and workflow output;
- and anything a note or report quotes out of any of the above.

**A repository is its history, not its tip.** Taking something out at the tip does not
unpublish it. Once a commit is pushed, the only remedies are a rewrite and the lead's
decision, so the cheap moment is before the commit, every time.

### It applies to passing remarks

**The subject of a commit does not limit the rule.** A commit about something unrelated
is checked exactly as carefully as one about the private material, because that is
where nobody is looking. An aside, a parenthesis, a "while I was at it", a name
mentioned only to explain something else — those are where this kind of thing slips
through, not in the places anyone is guarding.

### When you find something the guard missed

**Report it to the lead, and add it to the guard, in the same change.** Do not handle it
quietly and move on: a gap that is noticed and not closed is a gap that gets found again
the hard way. **Do not add it to this section as an example** — this section is public,
and an example here would publish the very thing the guard was just taught to stop.

### What is permitted, so this is not read as a gag

- **Compatibility is permitted. The method is not.** A public client or server may be
  fully compatible with the original. What may not appear is anything about how.
- **Saying that something is supplied by a private module is allowed**, and so is naming
  the public interface it plugs into. The cipher trait, the security hooks and the
  documentation describing their shape are this repository's own contents and belong
  here. They are what makes the rest work unchanged.
- **Saying the material exists and is being kept back is allowed**, including in public
  documentation, as long as the saying carries nothing that fails the test above. The
  public repositories already say it, and should.
- **The VIE-era stack and OpenSec are public in full**, constants, vectors and all.
  Nothing in this section narrows them, and nothing here is a reason to redact them.

### The mechanical check

A local guard refuses a commit whose staged content, message or ref names match a
private pattern list. **The list is not in this repository and must never be**: a
denylist of forbidden terms, committed to a public repository, is itself a disclosure —
the most efficient index to the problem anyone could write. It is kept outside this
repository, and the hooks that read it live in `.git/hooks`, which is neither committed
nor cloned.

Three things about it that matter more than its existence:

- **It fails closed.** If it cannot find or read its list it refuses the commit and says
  so. A check that passes when it cannot find its list is worse than no check at all.
- **It is a floor, not a ceiling.** It matches known shapes and has nothing to say about
  one nobody has thought of yet. **The test at the top of this section is the rule; the
  guard only catches the part of it that a regular expression can see.**
- **A clone does not have it.** Anyone working from a fresh clone, without the private
  list, has no guard at all — only this section and review. That is the price of not
  publishing the list, and it is the right way round: the written rule travels, the index
  does not.

Bypassing the guard with `--no-verify` is a decision to report to the lead, not a step.

### When in doubt

**It does not go in.** There is no deadline that trades against this. It cannot be
reasoned around case by case, and it does not expire. Ask the lead instead of deciding.

The reasoning behind all of the above is recorded as decisions **`D10`, `D17`, `D19`
and `D33`**. If you think one of them is wrong, that is a question for the lead rather
than an edit.

## If the local-only card is not beside this file

This repository is one of several developed together, and what is agreed across them —
conventions, decisions already taken, and work already in progress — is recorded outside
it, in a checkout that the local-only card beside this file names. That card is
deliberately not committed, so a working copy can arrive without it: a fresh clone does,
and so does a secondary working tree made from this one.

**A missing card is not evidence that there is no earlier work.** If you are working
here and it is not present, stop and ask for it rather than carrying on from this file
alone. Do not re-decide something that has already been settled, and do not start work
that may already belong to somebody else.

## The project's name

**This section is identical in all four public repositories. Change it in all four or
in none.**

The project is called Open SubSpace, written as two words with a capital S at the start
of Sub and of Space. Its repositories, crates, programs and folders carry the lower-case
prefix oss-, which is the three capitals of Open SubSpace: oss-core, oss-server,
oss-client, oss-bot. In text a player or operator reads, the name is written in full,
never as OSS on its own, because OSS usually means open-source software. SubSpace and
Continuum are the names of the original games; Open SubSpace is an independent,
compatible implementation and is not affiliated with their authors.

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
  `cargo fmt --check` since `fd0e90c` on 2026-11-12; read `.github/workflows/ci.yml` for what
  CI does today. Inside oss-core itself `--all` is correct and should stay.
  **And the writing form reformats every file in the workspace, including ones another
  session has uncommitted.** Harmless while one session worked at a time; not harmless now.
  On 2026-11-12 two sessions reformatted each other's in-progress files within one hour, in
  opposite directions, and neither could have known. **When you may not be alone in the
  checkout, run `cargo fmt --check` first** — it names the files it would change — and use
  the writing form only when that list is your own; verify your own files with
  `rustfmt --check --edition 2024 <file>`. Saying afterwards that someone else's file was
  dirty does not un-reformat it.
