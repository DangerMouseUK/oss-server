# What has connected to this server

Interoperability is the whole point of this server, so it is worth recording what has
actually joined it rather than what should. Each row below was a live session on loopback,
not a unit test; the detail underneath says what was observed and, as importantly, what was
*not* covered.

| Client | Encryption | Result | Date |
|---|---|---|---|
| **Continuum 0.40**, the genuine client | Continuum (cipher from a private module — see below) | Logged in, entered an arena, rendered the zone, flew ~96 s. Five sessions, no kick | 2026-11-12 |
| **MERVBot 4.6**, prebuilt and unmodified | VIE | Logged in, entered an arena, downloaded news and map, answered the security challenge, exchanged position packets | 2026-11-12 |
| **oss-client** (Open SubSpace) | VIE and OpenSec | Plays: arena, map, LVZ, chat, kills | continuously |
| **oss-bot** (headless, Open SubSpace) | VIE and OpenSec | Logs in, flies, chats; used for load swarms | continuously |
| **oss-conformance** `server` mode | VIE | 28 live protocol checks pass, 1 skipped | 2026-11-12 |

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

Everything except the Continuum row needs only these repositories.

```sh
# the server, bundled zone, UDP 5000
cargo run --release

# a third-party bot: point any VIE bot core at 127.0.0.1:5000

# the conformance suite's live server checks (from an oss-core checkout)
cargo run --release -p oss-conformance -- server 127.0.0.1:5000
```

The settings-packet vectors are in `crates/oss-server/tests/fixtures/asss-vectors/`, with the
header of each file naming the ASSS version and configuration it came from.
