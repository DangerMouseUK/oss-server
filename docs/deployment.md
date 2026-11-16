# Running a zone

Two ways to get `oss-server` onto a machine other than a developer's: a
container image ([`Dockerfile`](../Dockerfile)) and a script that installs it
as a systemd service ([`install.sh`](../install.sh)). This page is what an
operator needs to use either of them.

Neither of them starts or enables anything. Both finish by telling you the
commands to run.

## What the server actually needs

Short version: **nothing, until you give it a zone directory.** `oss-server`
with no arguments is a playable zone.
The whole of `zone/` — every `.conf`, the `smallmap.lvl` map and the `scf`
arena — is compiled into the binary, so there is no configuration file to
write and no map to find before it will start.

The longer version, because the difference matters once there are real players
in it:

| | |
|---|---|
| **Configuration files** | None at all with no `--zone` — the bundled copies are read from inside the binary. With `--zone <dir>`, **`conf/global.conf` is the one required file**: without it the server prints `config file not found: arena None, name "global.conf"` and exits, having bound nothing (measured against an empty directory). Everything else is optional and falls back to a built-in default: `conf/modules.conf`, `conf/passwd.conf`, `conf/staff.conf`, `arenas/<name>/arena.conf` and the `#include`s they pull in, read the way ASSS reads them. |
| **Ports** | UDP **5000** is the game port. UDP **5001** is the ping/population port, and it is always the game port **+ 1** — it is not separately configurable. TCP **5000** is the text chat protocol (`chatnet`), on the *same number* as the game port. All three move together if you change `[Listen] Port` in `conf/global.conf` or pass `--bind`. |
| **Writable directories** | Only when running with `--zone <dir>`. The server creates `<zone>/data` for its SQLite databases (`oss.db` for scores and persisted state, `bans.db`) and for `opensec.key`, the zone's long-term server identity. `<zone>/tmp` appears if a staff member uploads a file. `conf/passwd.conf` is appended to when a player sets a password with `?passwd`, so `conf/` wants to be writable too. Running with the bundled zone writes nothing at all — the databases are held in memory and the key is new on every start. |
| **Map and settings files** | Not needed to start. An operator can supply their own later by pointing `--zone` at an ASSS zone directory, or by starting from `oss-server --init-zone <dir>`, which writes the bundled zone out as an editable copy. |

### What the bundled zone is, and is not, good for

It is the ASSS 1.4.4 sample zone (see [`zone/README.md`](../zone/README.md)),
and it is genuinely playable: a client logs in, enters the `(default)` arena,
downloads `smallmap.lvl` and flies. Verified by running the server with no
arguments and connecting headless bots from `oss-client` to it.

It is **not** a zone you should leave facing the public:

- **Nothing persists.** Scores, bans and the server's OpenSec key live in
  memory and are gone at every restart, so the zone has a different identity
  each time it comes back.
- **Nobody can be made staff.** Group passwords are read from
  `conf/staff.conf`, and the bundled copy has none — with the zone inside the
  binary there is no file to put one in. `?grplogin` has nothing to log in to,
  and the staff commands that touch files (`?makearena`, `?putfile`,
  `?putmap`) refuse outright because there is no zone directory.
- **Anyone can log in as anyone.** `conf/passwd.conf` ships with
  `AllowUnknown = yes` and an empty `[Users]`, which means any name is accepted
  with any password.
- **One small map, one demo arena**, and the zone announces itself to no
  directory server.

So: fine for a first run, a LAN game, a demo or a test. For a real zone, give
it a directory — `install.sh` does that for you, and the container takes one on
a mount.

## The container image

The image builds the server from source in one stage and ships only the binary
in the next, on `debian:bookworm-slim`. It runs as an unprivileged user,
uid/gid **10001**.

`oss-server` takes its protocol and format crates from the sibling
[oss-core](https://github.com/DangerMouseUK/oss-core) checkout, which is a
Cargo *path* dependency. The build context here is `oss-server` alone, so the
builder stage clones oss-core itself:

```sh
docker build -t oss-server .
```

For a build you can reproduce later, pin the other repository to a tag or a
commit:

```sh
docker build --build-arg OSS_CORE_REF=<tag-or-sha> -t oss-server .
```

`RUST_VERSION` (default `1.100.0`, the toolchain CI gates on) and
`OSS_CORE_URL` are build arguments too.

### Running it

With no arguments it is the bundled zone, and nothing needs mounting:

```sh
docker run --rm -p 5000:5000/udp -p 5001:5001/udp -p 5000:5000/tcp oss-server
```

Read the caveats above before leaving that one running.

With your own zone, mount the directory and point the server at it. The
container writes into `<zone>/data`, so the mount has to be writable by
uid 10001:

```sh
sudo chown -R 10001:10001 /srv/myzone
docker run -d --name myzone --restart unless-stopped \
    -v /srv/myzone:/zone \
    -p 5000:5000/udp -p 5001:5001/udp -p 5000:5000/tcp \
    oss-server --zone /zone
```

To get a zone directory to mount in the first place, let the image write one:

```sh
docker run --rm -v /srv/myzone:/zone oss-server --init-zone /zone
```

`--init-zone` refuses to touch a directory that already holds a zone, so that
command cannot overwrite one.

Everything after the image name is passed straight to the server, so
`--bind`, `--log` and `--json` work the same way.

### Ports, again, because container port mapping is easy to get half right

`-p 5000:5000/udp` alone gives you a zone that players can join but that shows
up as unreachable to anything asking for its population, because that question
goes to UDP 5001. And the TCP mapping on 5000 is a different mapping from the
UDP one — Docker needs both spelled out.

Publish all three unless you do not want the chat protocol. To turn that one
off, drop `chatnet` from `conf/modules.conf`; to keep it but reachable only
locally, set `Net:ChatListen = 127.0.0.1:5000` in `conf/global.conf`. Note
that `--bind 127.0.0.1:5000` does **not** do that on its own: the chat
listener takes only the *port* from the game socket and binds `0.0.0.0`
regardless, so a server you believed was loopback-only still has a TCP port
open on every interface.

## `install.sh`

For a small Linux server where you want the zone running under systemd,
restarting on its own, as a user that is not root.

```sh
git clone https://github.com/DangerMouseUK/oss-core
git clone https://github.com/DangerMouseUK/oss-server
cd oss-server
cargo build --release
sudo ./install.sh
```

There are no published release binaries yet, so the script does not download
anything: it installs a binary you already built, or builds one for you with
`--build`. (It refuses to build as root, because that leaves root-owned files
in `target/`. Build as yourself, then run the installer with `sudo`.)

### Where it puts things

| | |
|---|---|
| `/usr/local/bin/oss-server` | the binary (`--prefix` moves it) |
| `/var/lib/oss-server/zone` | the zone: `conf/`, `arenas/`, `maps/`, and the `data/` the server creates (`--zone-dir`) |
| `/etc/oss-server/server.env` | extra command-line arguments for the unit (`--config-dir`) |
| `/etc/systemd/system/oss-server.service` | the unit (`--unit-dir`) |
| user and group `oss-server` | a system account with no login shell (`--user`, or `--no-user` to skip) |

The zone's configuration lives inside the zone directory rather than under
`/etc`, because ASSS's file lookup resolves `conf/`, `arenas/` and `maps/`
relative to one zone root and splitting them would break it. The port, the
zone name, the admin API and the rest are in
`/var/lib/oss-server/zone/conf/global.conf`.

`server.env` holds one variable, `OSS_SERVER_ARGS`, which systemd splits on
whitespace and appends to the command line — for `--bind`, `--log`, `--json`.

### Running it twice

It is meant to be re-run, and it says what it did and did not touch:

- **The binary** is replaced. That is the point of running it again.
- **The zone directory is never overwritten.** If `conf/global.conf` is already
  there, the whole directory is left alone and the script says so. (The binary
  refuses as well, so there are two locks on that door.)
- **`server.env`** is left alone. If the version the script would write differs
  from yours, it writes `server.env.new` beside it and tells you.
- **The unit file** is rewritten, because it is the installer's to maintain —
  *unless* you have edited it. The script writes a marker line into every unit
  it generates; if that line is gone, it keeps your file and writes
  `oss-server.service.new` instead. To customise the unit and still get
  updates, use a drop-in: `sudo systemctl edit oss-server`.
- **Directory modes** are set when the directory is created and never changed
  afterwards, so a mode you chose survives.

`--dry-run` prints every action and changes nothing.

### After it finishes

Nothing is started and nothing is enabled. The script ends by printing these,
and they are the whole of what is left:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now oss-server
journalctl -u oss-server -f
```

Before that, two things are worth doing:

1. **Give yourself staff.** Put a line under `[GroupPasswords]` in
   `zone/conf/staff.conf`:

   ```ini
   [GroupPasswords]
   sysop = <a password>
   ```

   then, in the game, `?grplogin sysop <that password>`.

2. **Decide who may log in.** `zone/conf/passwd.conf` ships with
   `AllowUnknown = yes`: any name, no password. Set it to `no` and list names,
   or point the zone at a user database server — `[Billing]` in
   `conf/global.conf`, and `oss-biller` in this repository is a small local one.

And open the ports: UDP 5000, UDP 5001, TCP 5000.

### What the unit does

`Restart=always` with `RestartSec=5`, so a crash brings it back; and
`StartLimitBurst=5` in a 60-second window, so a zone that cannot start at all
gives up instead of looping. `WantedBy=multi-user.target` is what makes
`systemctl enable` survive a reboot.

The service runs with `ProtectSystem=strict` and an empty capability set — the
only path it can write to is its own state directory, and it needs no
privileges because all three ports are above 1024. If your systemd is old
enough to warn about a directive in there (`ProtectProc` needs 247,
`ProtectClock` 245), the warning is harmless: systemd ignores keys it does not
know.

## Upgrading

Container: rebuild the image and recreate the container. The zone directory is
on the mount, so nothing in it moves.

systemd: build the new binary, re-run `sudo ./install.sh`, then
`sudo systemctl restart oss-server`. The installer replaces the binary and the
unit and leaves the zone and your environment file alone.

## What has been tested, and what has not

In keeping with the rest of this repository, the coverage rather than the
claim:

- **Tested** (2026-11-16, on Windows): the server starting with no arguments
  and no files and taking real clients; what a `--zone` directory must contain
  before the server will start; `--init-zone` writing a zone and
  refusing to overwrite one; `install.sh` run twice against a throwaway prefix,
  including the edited-config, edited-env-file and hand-edited-unit paths; and
  the installed binary serving the installed zone to a client.
- **Not tested**: the `Dockerfile` has never been built, and the systemd unit
  has never been loaded by systemd. No Docker and no systemd were available on
  the machine where they were written. Both are reasoned from the code and
  from the behaviour measured above, which is not the same thing as having run
  them. If you are the first to try either, an issue saying what broke is
  worth more than a polite one saying it worked.
