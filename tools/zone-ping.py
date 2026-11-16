#!/usr/bin/env python3
"""Ask a running zone whether it is alive, and how many players are in it.

Used by CI to tell a working zone from a process that has merely not exited.
A zone answers a 4-byte datagram on its ping port (the game port + 1) with
8 bytes: the population as a little-endian u32, then the 4 bytes it was sent
back unchanged. Anything else -- no answer, a short answer, an answer that
does not echo -- is not a zone.

  zone-ping.py 127.0.0.1 5001 --wait 60
  zone-ping.py 127.0.0.1 5001 --expect-players 1
  zone-ping.py 127.0.0.1 5001 --tcp-port 5000

Exit status is 0 if every check asked for passed, 1 otherwise, and the
reason is printed either way.
"""

from __future__ import annotations

import argparse
import os
import socket
import struct
import sys
import time

REQUEST_LEN = 4
REPLY_LEN = 8


def ping_once(host: str, port: int, timeout: float) -> int | None:
    """Returns the population, or None if there was no valid reply."""
    token = os.urandom(REQUEST_LEN)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.settimeout(timeout)
        try:
            s.sendto(token, (host, port))
            reply, _ = s.recvfrom(64)
        except (socket.timeout, OSError):
            return None
    if len(reply) != REPLY_LEN:
        print(f"  ping: {len(reply)} bytes back, wanted {REPLY_LEN}", file=sys.stderr)
        return None
    if reply[4:] != token:
        print("  ping: the reply did not echo the request", file=sys.stderr)
        return None
    return int(struct.unpack_from("<I", reply, 0)[0])


def tcp_reachable(host: str, port: int, timeout: float) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        return s.connect_ex((host, port)) == 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("host")
    p.add_argument("port", type=int, help="the ping port: the game port + 1")
    p.add_argument(
        "--wait",
        type=float,
        default=0.0,
        metavar="SECONDS",
        help="keep retrying for this long before giving up (default: one try)",
    )
    p.add_argument(
        "--expect-players",
        type=int,
        default=None,
        metavar="N",
        help="fail unless the zone reports at least N players",
    )
    p.add_argument(
        "--tcp-port",
        type=int,
        default=None,
        metavar="PORT",
        help="also check that a TCP connection to this port is accepted "
        "(the text chat protocol listens on the game port)",
    )
    args = p.parse_args()

    deadline = time.monotonic() + args.wait
    players = None
    while True:
        players = ping_once(args.host, args.port, timeout=1.0)
        if players is not None:
            break
        if time.monotonic() >= deadline:
            print(
                f"no zone answering on {args.host}:{args.port} "
                f"after {args.wait:.0f}s",
                file=sys.stderr,
            )
            return 1
        time.sleep(1.0)

    print(f"zone on {args.host}:{args.port} answered: {players} player(s)")

    ok = True
    if args.expect_players is not None and players < args.expect_players:
        print(
            f"expected at least {args.expect_players} player(s), got {players}",
            file=sys.stderr,
        )
        ok = False

    if args.tcp_port is not None:
        if tcp_reachable(args.host, args.tcp_port, timeout=5.0):
            print(f"chat protocol accepted a TCP connection on port {args.tcp_port}")
        else:
            print(
                f"nothing accepted a TCP connection on port {args.tcp_port}",
                file=sys.stderr,
            )
            ok = False

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
