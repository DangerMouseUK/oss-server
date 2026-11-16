# Container image for the Open SubSpace zone server.
#
#   docker build -t oss-server .
#   docker run --rm -p 5000:5000/udp -p 5001:5001/udp -p 5000:5000/tcp oss-server
#
# With no arguments the server runs the zone bundled into the binary, so the
# command above is a playable zone with nothing mounted. That zone keeps
# nothing across restarts and has no way to grant anyone staff powers; see
# docs/deployment.md before putting it in front of real players.
#
# It is not open, so you need one thing before you can connect. A zone whose
# operator configured no logins generates a single shared password at start and
# prints it. The bundled zone has nowhere to keep one, so it is new every run:
#
#   docker logs <container>   # the line beginning "no logins were configured"
#
# Players log in with any name and that password. Pass --zone-password to
# choose your own instead.
#
# To run your own zone directory, mount it and point the server at it:
#
#   docker run --rm -v /srv/myzone:/zone -p 5000:5000/udp -p 5001:5001/udp \
#       -p 5000:5000/tcp oss-server --zone /zone
#
# The server writes into <zone>/data and <zone>/tmp, so the mount has to be
# writable by uid 10001 (the image's unprivileged user).

# The toolchain the project's CI gates on; override to match a newer pin.
ARG RUST_VERSION=1.100.0
ARG DEBIAN_RELEASE=bookworm

# ---------------------------------------------------------------- build stage

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS build

# oss-server takes its protocol, transport and format crates from the sibling
# oss-core checkout (a Cargo path dependency, see Cargo.toml). The build
# context here is oss-server alone, so the builder fetches oss-core itself.
# Pin OSS_CORE_REF to a tag or commit for a reproducible image.
ARG OSS_CORE_URL=https://github.com/DangerMouseUK/oss-core.git
ARG OSS_CORE_REF=main

RUN apt-get update \
    && apt-get install --no-install-recommends --yes git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN git clone --depth 1 "${OSS_CORE_URL}" oss-core \
    && git -C oss-core fetch --depth 1 origin "${OSS_CORE_REF}" \
    && git -C oss-core checkout FETCH_HEAD

COPY . /src/oss-server
WORKDIR /src/oss-server

# Empty by default, so a casual `docker build` against oss-core's main branch
# is not refused for a lockfile that moved. A release build passes `--locked`
# here, for the reason .github/workflows/release.yml gives: what is handed to
# strangers should be the committed Cargo.lock or nothing.
ARG CARGO_FLAGS=

# rusqlite's `bundled` feature compiles SQLite from C; the rust image already
# carries a C toolchain, which is why this stage is not debian-slim.
RUN cargo build --release --package oss-server ${CARGO_FLAGS} \
    && strip target/release/oss-server

# -------------------------------------------------------------- runtime stage

FROM debian:${DEBIAN_RELEASE}-slim AS runtime

LABEL org.opencontainers.image.title="oss-server" \
      org.opencontainers.image.description="Open SubSpace zone server" \
      org.opencontainers.image.source="https://github.com/DangerMouseUK/oss-server" \
      org.opencontainers.image.licenses="GPL-2.0-or-later"

# A fixed uid so a bind-mounted zone directory can be chowned to a known
# number on the host without inspecting the image first. 10001 is outside
# Debian's system-uid range, so `useradd --system` prints "uid 10001 is
# greater than SYS_UID_MAX 999" while creating it exactly as asked. That
# warning in the build log is expected: a predictable uid an operator can
# chown to is worth more here than a tidy one nobody can guess.
RUN groupadd --system --gid 10001 oss \
    && useradd --system --uid 10001 --gid 10001 \
        --home-dir /var/lib/oss-server --shell /usr/sbin/nologin oss \
    && install -d -o oss -g oss -m 0750 /var/lib/oss-server

COPY --from=build /src/oss-server/target/release/oss-server /usr/local/bin/oss-server

# The game port, its ping port (game port + 1, UDP), and the text chat
# protocol, which listens on TCP on the *same* number as the game port.
EXPOSE 5000/udp
EXPOSE 5001/udp
EXPOSE 5000/tcp

USER oss
WORKDIR /var/lib/oss-server

# No CMD: with no arguments the server runs its bundled zone. Anything passed
# to `docker run` is appended as server arguments (--zone, --bind, --log ...).
ENTRYPOINT ["/usr/local/bin/oss-server"]
