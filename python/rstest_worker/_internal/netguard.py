"""Outbound-network guard: `--block-network` fails a test that connects outside
loopback and the allowlist; `--doctor` only records each test's destinations.

Patches `socket.socket.connect` / `connect_ex` (every TCP/UDP client path ends
there) and `socket.getaddrinfo` (only to remember which hostname an address came
from, so messages name `api.github.com:443`, not a bare IP). Only connections
made while a test runs (setup, call, teardown) are checked; collection and
session-level code are left alone. A blocked call raises `NetworkBlocked`, a
RuntimeError: an `except OSError` retry loop must not swallow it.
"""

from __future__ import annotations

import contextlib
import ipaddress
import os
import socket
import traceback
from pathlib import Path
from typing import Any

# Destinations kept per test (the doctor report lists the first few).
MAX_PER_TEST = 20


class NetworkBlocked(RuntimeError):
    """A test connected outside loopback and the allowlist under --block-network."""


class NetGuard:
    def __init__(self, block: bool, allow: list[str], rootpath: Any) -> None:
        self.block = block
        self.hosts = {a.strip().lower() for a in allow if a.strip()}
        self.nets = []
        for a in self.hosts:
            with contextlib.suppress(ValueError):
                self.nets.append(ipaddress.ip_network(a, strict=False))
        self.root = Path(rootpath).resolve()
        self.names: dict[str, str] = {}
        self.current: str | None = None
        self.exempt = False
        self.seen: dict[str, list[str]] = {}

    @classmethod
    def from_env(cls, config: Any) -> NetGuard | None:
        """The guard the orchestrator asked for, or None (nothing is read from
        `config` then: the guard costs nothing when off)."""
        mode = os.environ.get("RSTEST_NETWORK")
        if mode not in ("block", "record"):
            return None
        allow = os.environ.get("RSTEST_NETWORK_ALLOW", "").split("\n")
        return cls(mode == "block", allow, config.rootpath)

    def install(self) -> None:
        guard = self
        connect = socket.socket.connect
        connect_ex = socket.socket.connect_ex
        getaddrinfo = socket.getaddrinfo

        def guarded_getaddrinfo(host, *args, **kwargs):
            res = getaddrinfo(host, *args, **kwargs)
            if isinstance(host, str):
                for *_, sockaddr in res:
                    guard.names.setdefault(str(sockaddr[0]), host)
            return res

        def guarded_connect(sock, address):
            guard.check(sock, address)
            return connect(sock, address)

        def guarded_connect_ex(sock, address):
            guard.check(sock, address)
            return connect_ex(sock, address)

        # Deliberate monkeypatch of the stdlib entry points.
        socket.getaddrinfo = guarded_getaddrinfo  # ty: ignore[invalid-assignment]
        socket.socket.connect = guarded_connect
        socket.socket.connect_ex = guarded_connect_ex

    def allowed(self, host: str, name: str) -> bool:
        if name.lower() in ("localhost", *self.hosts) or host.lower() in self.hosts:
            return True
        try:
            ip = ipaddress.ip_address(host.split("%", 1)[0])
        except ValueError:
            return False
        return ip.is_loopback or ip.is_unspecified or any(ip in n for n in self.nets)

    def call_site(self) -> str | None:
        """`path:line` of the deepest stack frame in the project (the rootdir,
        outside site-packages) below this module."""
        here = Path(__file__).resolve()
        for frame in reversed(traceback.extract_stack()):
            try:
                path = Path(frame.filename).resolve()
                rel = path.relative_to(self.root)
            except (ValueError, OSError):
                continue
            if path == here or "site-packages" in rel.parts:
                continue
            return f"{rel.as_posix()}:{frame.lineno}"
        return None

    def check(self, sock: Any, address: Any) -> None:
        if self.current is None or not isinstance(address, tuple) or len(address) < 2:
            return  # outside a test, or an AF_UNIX path
        if sock.family not in (socket.AF_INET, socket.AF_INET6):
            return
        host, port = str(address[0]), address[1]
        name = self.names.get(host, host)
        if self.allowed(host, name):
            return
        dest = f"{name}:{port}" if name == host else f"{name}:{port} ({host})"
        dests = self.seen.setdefault(self.current, [])
        if dest not in dests and len(dests) < MAX_PER_TEST:
            dests.append(dest)
        if self.block and not self.exempt:
            site = self.call_site()
            where = f" from {site}" if site else ""
            raise NetworkBlocked(
                f"rstest --block-network: connection to {dest}{where}; allow it with "
                "@pytest.mark.network or [tool.rstest] network-allow"
            )

    def start(self, item: Any) -> None:
        self.current = item.nodeid
        self.exempt = item.get_closest_marker("network") is not None

    def stop(self) -> None:
        self.current = None
        self.exempt = False

    def pop(self, nodeid: str) -> list[str]:
        return self.seen.pop(nodeid, [])
