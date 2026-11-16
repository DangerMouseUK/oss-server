# What has connected to this server

Interoperability is the whole point of this server, so it is worth recording what has
actually joined it rather than what should. Each row below was a live session on loopback,
not a unit test; the detail underneath says what was observed and, as importantly, what was
*not* covered.

| Client | Encryption | Result | Last run |
|---|---|---|---|
| **Continuum 0.40**, the genuine client | Continuum (cipher from a private module — see below) | Logged in, entered an arena, rendered the zone, flew ~96 s. Five sessions, no kick | 2026-11-12 |
| **MERVBot 4.6**, prebuilt and unmodified | VIE | Logged in, entered an arena, downloaded news and map, answered the security challenge, exchanged position packets | 2026-11-12 |
| **oss-client** (Open SubSpace) | VIE and OpenSec | Plays: arena, map, LVZ, chat, kills | continuously |
| **oss-bot** (headless, Open SubSpace) | VIE and OpenSec | Logs in, flies, chats; used for load swarms | continuously |
| **oss-conformance** `server` mode | VIE | 30 live protocol checks pass, 0 fail, 1 skipped | 2026-11-15 |
| **SubSpace 1.34**, the original 1998 client | VIE | Logged in, downloaded news, entered an arena and flew, on an ordinary account holding no capabilities. Staff-only until a login-response field was corrected on 2026-11-14 | 2026-11-14 |

**Every client in that table encrypts, and from 2026-11-16 that is a requirement rather than
an observation.** A zone answers a connection request only with an encrypted session: VIE at
the least, OpenSec or a private module where both ends have one. A peer that asks for no
encryption — a connection request carrying a key of zero, which ASSS honours and the original
1998 server does not — is declined, and gets no answer at all. The exception is deliberate and
has to be configured: `Security:AllowUnencrypted` in `conf/global.conf` registers an acceptor
for unencrypted sessions behind every encrypting one, for development. The client has to have
asked for one too, so an unencrypted session is a decision at each end. `D28` has the reasoning
and what it costs; `oss-core`'s `docs/protocol/encryption.md` has the mechanics.

**The last column is when that row was last run** — not the date of anything described
inside it. Where a row has a history worth keeping, such as a defect found on one date and
fixed on another, both dates are in that row's section below rather than in the table.

## Continuum 0.40

A genuine `Continuum.exe` (version 0.40) completed five live sessions against a server built
from this repository's library with a private Continuum cipher module registered on top.
It sent the real 165-byte `0x24` login packet, was authenticated, entered an arena, drew the
map, flags and radar, and stayed in for about 96 seconds sending one position packet a
second. It answered the `0x1A` security challenge, and the server logged no settings, map or
executable checksum mismatch and did not kick it. The client also compared the login reply's
news checksum against its cached `news.txt` and, when they differed, put a Yes/No prompt to
the player — sending the `0x0D` request only on *Yes*, which the server handles either way.

**Read this honestly.** Continuum's key expansion is **not in this repository and will not
be**; see [the limits section of the README](../README.md#what-works-and-what-does-not). The
session above used a private module that registers its own `oss_net::Acceptor` and
`oss_net::Cipher` through the hooks described in [architecture.md](architecture.md#transport-and-security-hooks),
with no change to any code here. So what this result demonstrates is two things: that the
hooks are sufficient for a real Continuum handshake, and that **everything above the cipher**
— login, arena entry, settings, map download, news, position relay, the security exchange —
is right enough for the original client to play on. It does not mean you can point a
Continuum client at a build of this repository.

One caveat on "the security check passed": it is read from the absence of any
`malicious(...)` warning and the absence of a kick. The explicit `ok=` line is logged at
`security=debug`, which was not enabled for those runs.

## MERVBot 4.6

A prebuilt **MERVBot 4.6** binary — not recompiled, byte-identical to the reference copy,
with only `Zone=`, `NoTerminal` and `ChatterLog` set in its `MERVBot.INI` — logged in to
`oss-server` over VIE on loopback. It authenticated under `AllowUnknown`, entered arena `0`,
downloaded `news.txt` and `smallmap.lvl` and wrote them to disk, parsed the arena settings,
answered the `security` challenge with `ok=true`, received public chat, and exchanged
position packets in both directions — its own C2S `0x03` stream and a continuous S2C `0x05`
stream for a second player.

Two differences from the original server did not trouble it: our `00 02` is 6 bytes where
subgame's is 7, and we send no `00 05` liveness challenge. Three of its startup commands
(`?mod`, `?relkills`, `?chat`) got no reply — which is what ASSS 1.4.4 does with the same
module set, since `cmdman.c` has no default handler, `?chat` belongs to the biller, and the
other two exist nowhere in ASSS. The bot carried on past all three.

**Not covered:** spectator only, so nothing ship-side was exercised by this client; VIE
transport only; MERVBot 4.8 and TWCore were not run; the second player in the arena was our
own `oss-bot`.

## SubSpace 1.34

The original 1998 `SUBSPACE.EXE` (630,784 bytes, VIE 1.34 by its version resource)
connected over VIE, logged in, downloaded `news.txt`, entered an arena and was flown. The
first session, on 2026-11-13, was a staff account and lasted about a minute; an ordinary
account got a different answer until 2026-11-14, which is the rest of this section. It
answered the `0x1A` security challenge with the settings and map checksums correct and
key-dependent, and with `exe_checksum = 0` — it reports no executable checksum at all,
which is why the server reads a zero there as "not applicable" rather than as a mismatch.

**Ordinary accounts can log in, and that was only made true on 2026-11-14.** For two
days a player without `CAP_SEE_PRIV_FREQ` answered the login response with
`SEC_LoginChecksumMismatch` (`0x17`) and the process exited about two milliseconds
later, while staff logged in without trouble — the staff path sends `0xFFFF_FFFF` for
both login checksums, which the client skips, so the failure was invisible to anyone
testing as staff.

**The cause was one field.** The login response's `code_checksum` is the exe checksum
computed with a checksum key of zero, and this server was sending zero itself. Zero is
not "not applicable" in that direction: the client computes the value and compares it,
so zero is a mismatch. Sending the computed value — `0x281CC948` for `SUBSPACE.EXE`
1.34 — is the whole of the fix.

**Demonstrated, 2026-11-14.** An account holding no capabilities logged in, entered an
arena, flew for 226 seconds and answered four security checks, then quit cleanly. The
zone had no `scrty` key table and `SecurityKickoff = 1`, so every check was drawn from
the fallback path and scored with ejection armed.

**Read the rest of this row honestly.**

- **What was not covered:** one person flew, twice on 2026-11-14, for about four minutes
  each — the measured session is the 226 seconds above. Login, arena entry, settings, map
  download, news and the security exchange are demonstrated. Weapons, kills, flags, chat
  and a second player are not, and SubSpace 1.35 has never been started.
- **One trailing field is still unexplained.** The original server puts a `0x56` in the
  second word of the login response's unused block where this server sends zero. A flight
  with it removed succeeded, so the client does not need it; this server therefore does
  not send it. What it is for is not known, and nothing here guesses.

## The conformance suite

`oss-conformance server` drives a running zone as a VIE client and checks what comes back.
Against a build of this repository at `e4e648c` with oss-core at `0a21e82`, on the bundled
default zone: **30 passed, 0 failed, 1 skipped**, exit 0. Two runs on 2026-11-15, the same
both times. The one skip is `server.session security` — "the server sent no checksum request
within 2 s" — so that exchange is evidenced by the client rows above rather than here. The
command is in [Reproducing the parts you can](#reproducing-the-parts-you-can).

**That count was 28 until 2026-11-15, and the clean result behind it was worth less than it
looked.** The suite had sent the login response's code checksum to clients under test since
2026-11-08, but never read the field back from a server under test. So on 2026-11-12 it
reported 28 passed against a zone that no ordinary 1998 client could log in to, and it had no
means of saying otherwise. oss-core `0a21e82` added the two checks that read that pair, which
is where the two extra passes come from. The lesson is worth more than the number: **a suite
is evidence only for the fields it actually compares**, and a passing run bounds nothing
outside them.

## The client settings packet

The S2C `0x0F` settings packet is 1,428 bytes of densely bit-packed arena settings, and
getting one bit wrong gives a client that plays subtly differently from everyone else's. ASSS
1.4.4's own `clientset.c`, compiled unmodified, was used to build the packet for two real
configurations — the ASSS sample zone's public arena and a subgame `svs.cfg` — and the
results are recorded byte for byte as fixtures.

[`crates/oss-server/tests/asss_vectors.rs`](../crates/oss-server/tests/asss_vectors.rs)
checks, on every `cargo test` run, that each of 846 named settings sits at the bit offset and
length ASSS puts it at, that our reads of ASSS's packet return the values ASSS wrote, and
that our settings checksum matches ASSS's `GetChecksum` for seven different keys — for both
configurations. Set `OSS_ASSS_ZONE_DIR` to an ASSS 1.4.4 zone directory (or `OSS_SUBGAME_DIR`
to a subgame directory containing `svs.cfg`) and the same test additionally **rebuilds the
whole packet from that config and asserts it is byte-identical** to ASSS's. Those two
assertions are skipped without the directories, which is why CI does not run them: the zones
cannot be shipped here.

The same file checks the position checksum rules (C2S `0x03` and S2C `0x05`) against ASSS,
and the map file CRC-32s the server reports in S2C `0x29`.

## Reproducing the parts you can

Everything except the Continuum row needs nothing private — though the third-party client
rows also need that client, which this repository does not ship. Only the Continuum row
depends on a module that is not public and will not be.

```sh
# the server, bundled zone, UDP 5000
cargo run --release

# a third-party bot: point any VIE bot core at 127.0.0.1:5000

# the conformance suite's live server checks (from an oss-core checkout)
cargo run --release -p oss-conformance -- server 127.0.0.1:5000
```

The settings-packet vectors are in `crates/oss-server/tests/fixtures/asss-vectors/`, with the
header of each file naming the ASSS version and configuration it came from.
