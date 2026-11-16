#!/usr/bin/env bash
#
# install.sh - put oss-server on a Linux server as a systemd service.
#
# It installs a binary, a zone directory, an environment file and a systemd
# unit, running the zone as a dedicated unprivileged system user. It does not
# start or enable anything: it prints the commands for that at the end.
#
# It is safe to run twice. The binary and the unit are replaced; an existing
# zone directory, an existing environment file and a hand-edited unit are
# left exactly as they are and reported.
#
#   sudo ./install.sh                 # install, using target/release/oss-server
#   sudo ./install.sh --build         # build it first with cargo, then install
#   sudo ./install.sh --binary ./oss-server
#   ./install.sh --dry-run            # say what would happen, change nothing
#
# See docs/deployment.md for what to do after it finishes.

set -euo pipefail

PROGRAM=oss-server
SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

# ------------------------------------------------------------------ defaults

PREFIX=/usr/local
STATE_DIR=/var/lib/oss-server
ZONE_DIR=
CONFIG_DIR=/etc/oss-server
UNIT_DIR=/etc/systemd/system
SERVICE_USER=oss-server
SERVICE_GROUP=
BINARY=
DO_BUILD=no
DO_USER=yes
DRY_RUN=no
ALLOW_ROOT_BUILD=no

# A line we write into the unit so a later run can tell its own unit from one
# the operator has taken over and edited.
UNIT_MARKER="# Managed by oss-server install.sh - edit with a drop-in instead"

usage() {
    cat <<EOF
Usage: $0 [options]

  --binary PATH       install this already-built binary
  --build             run 'cargo build --release' first (needs the sibling
                      oss-core checkout; see README "Building")
  --prefix DIR        binary goes in DIR/bin  (default: $PREFIX)
  --state-dir DIR     service state and zone  (default: $STATE_DIR)
  --zone-dir DIR      zone directory          (default: <state-dir>/zone)
  --config-dir DIR    environment file        (default: $CONFIG_DIR)
  --unit-dir DIR      systemd unit            (default: $UNIT_DIR)
  --user NAME         service account         (default: $SERVICE_USER)
  --group NAME        service group           (default: same as --user)
  --no-user           do not create or chown to a service account
  --allow-root-build  permit --build as root (leaves root-owned build output)
  --dry-run           print every action, change nothing
  -h, --help          this text

With no --binary and no --build, the script expects
$SELF_DIR/target/release/$PROGRAM to exist already.
EOF
}

# -------------------------------------------------------------- argument parse

while [ $# -gt 0 ]; do
    case $1 in
        --binary)           BINARY=${2:?--binary needs a path}; shift 2 ;;
        --build)            DO_BUILD=yes; shift ;;
        --prefix)           PREFIX=${2:?--prefix needs a directory}; shift 2 ;;
        --state-dir)        STATE_DIR=${2:?--state-dir needs a directory}; shift 2 ;;
        --zone-dir)         ZONE_DIR=${2:?--zone-dir needs a directory}; shift 2 ;;
        --config-dir)       CONFIG_DIR=${2:?--config-dir needs a directory}; shift 2 ;;
        --unit-dir)         UNIT_DIR=${2:?--unit-dir needs a directory}; shift 2 ;;
        --user)             SERVICE_USER=${2:?--user needs a name}; shift 2 ;;
        --group)            SERVICE_GROUP=${2:?--group needs a name}; shift 2 ;;
        --no-user)          DO_USER=no; shift ;;
        --allow-root-build) ALLOW_ROOT_BUILD=yes; shift ;;
        --dry-run)          DRY_RUN=yes; shift ;;
        -h|--help)          usage; exit 0 ;;
        *)                  printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

: "${ZONE_DIR:=$STATE_DIR/zone}"
: "${SERVICE_GROUP:=$SERVICE_USER}"
BIN_DIR=$PREFIX/bin
INSTALLED_BIN=$BIN_DIR/$PROGRAM
ENV_FILE=$CONFIG_DIR/server.env
UNIT_FILE=$UNIT_DIR/$PROGRAM.service

# ----------------------------------------------------------------- reporting

CHANGED=()
KEPT=()

say()     { printf '%s\n' "$*"; }
step()    { printf '\n== %s\n' "$*"; }
note()    { printf '   %s\n' "$*"; }
changed() { CHANGED+=("$1"); printf '   %s\n' "$1"; }
kept()    { KEPT+=("$1"); printf '   %s\n' "$1"; }
die()     { printf '%s: %s\n' "$PROGRAM install" "$*" >&2; exit 1; }

# Runs a command, or describes it under --dry-run.
run() {
    if [ "$DRY_RUN" = yes ]; then
        printf '   would run: %s\n' "$*"
    else
        "$@"
    fi
}

# ensure_dir <path> <mode> - create it with that mode; an existing directory
# keeps whatever mode the operator gave it, so a second run changes nothing.
ensure_dir() {
    local path=$1 mode=$2
    if [ -d "$path" ]; then
        return 0
    fi
    run mkdir -p "$path"
    if [ "$DRY_RUN" = yes ]; then
        printf '   would run: chmod %s %s\n' "$mode" "$path"
    elif ! chmod "$mode" "$path"; then
        note "warning: could not set mode $mode on $path - check it yourself"
    fi
}

# write_file <path> <mode> < content-on-stdin
write_file() {
    local path=$1 mode=$2 tmp
    if [ "$DRY_RUN" = yes ]; then
        cat >/dev/null
        printf '   would write: %s (mode %s)\n' "$path" "$mode"
        return 0
    fi
    tmp=$(mktemp "$path.XXXXXX")
    cat >"$tmp"
    chmod "$mode" "$tmp"
    mv -f "$tmp" "$path"
}

# ------------------------------------------------------------------ preflight

step "Checking the machine"

[ "$(uname -s)" = Linux ] || note "warning: this is $(uname -s), not Linux - the systemd parts will not apply"

HAVE_SYSTEMD=no
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    HAVE_SYSTEMD=yes
    note "systemd is running"
else
    note "no running systemd found - the unit will be written but is for you to use elsewhere"
fi

IS_ROOT=no
if [ "$(id -u)" -eq 0 ]; then
    IS_ROOT=yes
fi

# Every directory we are about to create or write into must be reachable.
for d in "$BIN_DIR" "$STATE_DIR" "$CONFIG_DIR" "$UNIT_DIR"; do
    probe=$d
    while [ ! -d "$probe" ] && [ "$probe" != "/" ] && [ "$probe" != "." ]; do
        probe=$(dirname "$probe")
    done
    if [ "$DRY_RUN" = no ] && [ ! -w "$probe" ]; then
        die "cannot write to $probe (for $d). Run this with sudo, or pass --prefix/--state-dir/--config-dir/--unit-dir somewhere you own."
    fi
done

if [ "$DO_USER" = yes ] && [ "$IS_ROOT" = no ] && [ "$DRY_RUN" = no ]; then
    die "creating the $SERVICE_USER account needs root. Run with sudo, or pass --no-user to skip it."
fi

# --------------------------------------------------------------- the binary

step "Finding the server binary"

if [ "$DO_BUILD" = yes ]; then
    if [ -n "$BINARY" ]; then
        die "--build and --binary are alternatives, not both"
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        die "--build needs cargo on PATH (https://rustup.rs)"
    fi
    if [ "$IS_ROOT" = yes ] && [ "$ALLOW_ROOT_BUILD" = no ]; then
        die "refusing to build as root: it leaves root-owned files in target/. Build as yourself with 'cargo build --release', then re-run this with sudo. Pass --allow-root-build to override."
    fi
    note "building in $SELF_DIR (this takes a few minutes the first time)"
    run env -C "$SELF_DIR" cargo build --release --package "$PROGRAM"
    BINARY=$SELF_DIR/target/release/$PROGRAM
elif [ -z "$BINARY" ]; then
    BINARY=$SELF_DIR/target/release/$PROGRAM
fi

# Checked under --dry-run too: without it the dry run dies later on a failed
# redirect instead of saying this, and a dry run with no binary to install
# would not be telling you much anyway.
if [ ! -x "$BINARY" ]; then
    die "no binary at $BINARY. Build it with 'cargo build --release' (see README \"Building\": oss-core must be checked out beside this repository), or pass --binary PATH, or re-run with --build."
fi
note "using $BINARY"

# ------------------------------------------------------------ service account

step "Service account"

if [ "$DO_USER" = no ]; then
    note "skipped (--no-user); files will keep their current ownership"
    SERVICE_USER=
    SERVICE_GROUP=
elif id -u "$SERVICE_USER" >/dev/null 2>&1; then
    kept "user $SERVICE_USER already exists - left alone"
else
    if command -v useradd >/dev/null 2>&1; then
        run groupadd --system "$SERVICE_GROUP" || true
        run useradd --system --gid "$SERVICE_GROUP" --home-dir "$STATE_DIR" \
            --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
    elif command -v adduser >/dev/null 2>&1; then
        run addgroup --system "$SERVICE_GROUP" || true
        run adduser --system --ingroup "$SERVICE_GROUP" --home "$STATE_DIR" \
            --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
    else
        die "no useradd or adduser on this system. Create the $SERVICE_USER account yourself and re-run with --no-user."
    fi
    changed "created the system user $SERVICE_USER"
fi

# ------------------------------------------------------------------- install

step "Binary"

ensure_dir "$BIN_DIR" 0755
if [ -e "$INSTALLED_BIN" ]; then
    WORD=replaced
else
    WORD=installed
fi
# Written to a temporary name and renamed into place, not copied over the top.
# Linux refuses to open a running executable for writing (ETXTBSY), so copying
# would fail on exactly the case this script is for: upgrading a zone that is
# currently up. A rename replaces the directory entry and leaves the running
# process on the old inode until it is restarted.
write_file "$INSTALLED_BIN" 0755 <"$BINARY"
changed "$WORD $INSTALLED_BIN"

step "Zone directory"

ensure_dir "$STATE_DIR" 0750
if [ -e "$ZONE_DIR/conf/global.conf" ]; then
    kept "$ZONE_DIR already holds a zone - not touched (no config or map of yours was overwritten)"
else
    ensure_dir "$ZONE_DIR" 0750
    # The binary refuses to write into a directory that already has a zone,
    # so this cannot clobber one even if the check above is wrong.
    run "$BINARY" --init-zone "$ZONE_DIR"
    changed "wrote the bundled default zone into $ZONE_DIR"
fi

if [ -n "$SERVICE_USER" ]; then
    run chown -R "$SERVICE_USER:$SERVICE_GROUP" "$STATE_DIR"
    note "$STATE_DIR is owned by $SERVICE_USER"
fi

step "Environment file"

ensure_dir "$CONFIG_DIR" 0755
env_template() {
    cat <<EOF
# Extra command-line arguments for $PROGRAM, split on whitespace by systemd.
# The zone directory is already passed by the unit, so this is for the rest:
#
#   --bind 0.0.0.0:5000     listen somewhere other than [Listen] Port
#   --log "info,net=debug"  log filter, RUST_LOG syntax
#   --json                  log as JSON lines
#
# The port, the zone name and everything else about the zone itself live in
# $ZONE_DIR/conf/global.conf, not here.
OSS_SERVER_ARGS=
EOF
}

if [ -e "$ENV_FILE" ]; then
    if env_template | cmp -s - "$ENV_FILE"; then
        kept "$ENV_FILE is already what this installer writes - left alone"
    else
        env_template | write_file "$ENV_FILE.new" 0640
        kept "$ENV_FILE exists and differs - yours kept, mine written to $ENV_FILE.new"
    fi
else
    env_template | write_file "$ENV_FILE" 0640
    changed "wrote $ENV_FILE"
fi

step "systemd unit"

unit_template() {
    local user_lines=""
    if [ -n "$SERVICE_USER" ]; then
        user_lines="User=$SERVICE_USER
Group=$SERVICE_GROUP
"
    fi
    cat <<EOF
$UNIT_MARKER
#   sudo systemctl edit $PROGRAM
# writes an override in $UNIT_DIR/$PROGRAM.service.d/ that survives a re-install.

[Unit]
Description=Open SubSpace zone server
Documentation=https://github.com/DangerMouseUK/oss-server
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=exec
${user_lines}WorkingDirectory=$STATE_DIR
EnvironmentFile=-$ENV_FILE
ExecStart=$INSTALLED_BIN --zone $ZONE_DIR \$OSS_SERVER_ARGS
Restart=always
RestartSec=5s
KillSignal=SIGTERM
TimeoutStopSec=30s

# The zone's own directory is the only thing it needs to write to. The game
# and ping ports are above 1024, so it needs no capabilities at all.
NoNewPrivileges=yes
CapabilityBoundingSet=
AmbientCapabilities=
ProtectSystem=strict
ReadWritePaths=$STATE_DIR
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectClock=yes
ProtectHostname=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectProc=invisible
# AF_NETLINK and AF_UNIX are here for name resolution, not for the game: on
# glibc, getaddrinfo opens a netlink socket to see which address families the
# machine actually has. Without them a zone works fine on the bundled config,
# which names no hosts, and then fails the day its operator points [Billing]
# or [Directory] at a hostname.
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
UMask=0077

[Install]
WantedBy=multi-user.target
EOF
}

ensure_dir "$UNIT_DIR" 0755
if [ -e "$UNIT_FILE" ] && ! grep -qF "$UNIT_MARKER" "$UNIT_FILE" 2>/dev/null; then
    unit_template | write_file "$UNIT_FILE.new" 0644
    kept "$UNIT_FILE was not written by this installer - yours kept, mine written to $UNIT_FILE.new"
elif [ -e "$UNIT_FILE" ]; then
    unit_template | write_file "$UNIT_FILE" 0644
    changed "updated $UNIT_FILE"
else
    unit_template | write_file "$UNIT_FILE" 0644
    changed "wrote $UNIT_FILE"
fi

# -------------------------------------------------------------------- summary

step "Done"

if [ ${#CHANGED[@]} -eq 0 ]; then
    note "nothing changed"
fi
if [ ${#KEPT[@]} -gt 0 ]; then
    note "${#KEPT[@]} thing(s) already existed and were left as they were - see above"
fi

cat <<EOF

Nothing has been started or enabled. Three things are still yours to do.

1. Look at the zone's settings, in $ZONE_DIR/conf -
   especially who may log in:

     conf/global.conf  the port, the zone name, the admin API
     conf/passwd.conf  who may log in. You have configured nobody, so the
                       first time the zone starts it makes up one shared
                       password, prints it, and writes it here. Players log
                       in with any name and that password, and it makes
                       nobody staff. After starting it, read it with:
                         sudo grep ZonePassword $ZONE_DIR/conf/passwd.conf
     conf/staff.conf   nobody is staff yet. Put a password under
                       [GroupPasswords] as 'sysop = <password>', then use
                       '?grplogin sysop <password>' once you are in the game

2. Open the ports. The game port is UDP 5000, its ping port is UDP 5001,
   and the text chat protocol listens on TCP 5000. For example:

     sudo ufw allow 5000/udp && sudo ufw allow 5001/udp && sudo ufw allow 5000/tcp

3. Start it, and have it come back after a reboot:

     sudo systemctl daemon-reload
     sudo systemctl enable --now $PROGRAM
     systemctl status $PROGRAM
     journalctl -u $PROGRAM -f

EOF

if [ "$HAVE_SYSTEMD" = no ]; then
    note "(systemd is not running here, so step 3 is for the machine this unit ends up on.)"
fi
if [ "$DRY_RUN" = yes ]; then
    note "(--dry-run: nothing above was actually done.)"
fi
