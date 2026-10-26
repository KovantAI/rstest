"""Outbound-network recorder for `--doctor`: lists each test's destinations
outside loopback and the allowlist. It never blocks; pytest-socket
(`--disable-socket --allow-hosts=...`) does that.

Patches `socket.socket.connect` / `connect_ex` (every TCP/UDP client path ends
there) and `socket.getaddrinfo` (only to remember which hostname an address came
from, so the report names `api.github.com:443`, not a bare IP). Only connections
made while a test runs (setup, call, teardown) are recorded; collection and
session-level code are left alone.
"""

from __future__ import annotations

import contextlib
import ipaddress
import os
import socket
from typing import Any

# Destinations kept per test (the doctor report lists the first few).
MAX_PER_TEST = 20


class NetGuard:
    def __init__(self, allow: list[str]) -> None:
        self.hosts = {a.strip().lower() for a in allow if a.strip()}
        self.nets = []
        for a in self.hosts:
            with contextlib.suppress(ValueError):
                self.nets.append(ipaddress.ip_network(a, strict=False))
        self.names: dict[str, str] = {}
        self.current: str | None = None
        self.seen: dict[str, list[str]] = {}

    @classmethod
    def from_env(cls) -> NetGuard | None:
        """The recorder the orchestrator asked for, or None."""
        if os.environ.get("RSTEST_NETWORK") != "record":
            return None
        return cls(os.environ.get("RSTEST_NETWORK_ALLOW", "").split("\n"))

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

    def start(self, nodeid: str) -> None:
        self.current = nodeid

    def stop(self) -> None:
        self.current = None

    def pop(self, nodeid: str) -> list[str]:
        return self.seen.pop(nodeid, [])
