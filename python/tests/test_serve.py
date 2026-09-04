"""Unit tests for serve-mode dispatch: overlay lifecycle, the forked-child
report/count plumbing, and the template plugin's request loop. The fork itself
is exercised end-to-end by e2e/gate.py; here we test the pure seams around it."""

from __future__ import annotations

import contextlib
import os
from types import SimpleNamespace
from typing import Any

from rstest_worker._internal import dispatch
from rstest_worker._internal.dispatch import (
    ServeDispatchPlugin,
    _apply_overlay,
    _restore_overlay,
    _ServeChildPlugin,
)


class FakeConn:
    """Records send()s and hands out queued recv_one() messages."""

    def __init__(self, incoming: list[dict[str, Any]] | None = None) -> None:
        self.sent: list[tuple[str, Any]] = []
        self._incoming = list(incoming or [])

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))

    def recv_one(self) -> dict[str, Any] | None:
        return self._incoming.pop(0) if self._incoming else None


def mk_report(
    when: str, outcome: str, *, nodeid: str = "t.py::a", failed: bool = False
) -> SimpleNamespace:
    return SimpleNamespace(
        nodeid=nodeid,
        when=when,
        outcome=outcome,
        duration=0.0,
        longreprtext="",
        longrepr=None,
        failed=failed,
        skipped=(outcome == "skipped"),
        sections=[],
        location=(nodeid.split("::")[0], 1, "a"),
    )


# ── overlay lifecycle ──────────────────────────────────────────────────────


def test_overlay_overwrites_and_restores_existing(tmp_path, monkeypatch):
    f = tmp_path / "mod.py"
    f.write_text("VAL = 1\n")
    monkeypatch.chdir(tmp_path)

    saved = _apply_overlay({"mod.py": "VAL = 999\n"})
    assert f.read_text() == "VAL = 999\n"

    _restore_overlay(saved)
    assert f.read_text() == "VAL = 1\n"


def test_overlay_preserves_exact_bytes(tmp_path, monkeypatch):
    # Restore must be byte-exact, not text-normalized (CRLF, no trailing NL).
    f = tmp_path / "mod.py"
    original = b"VAL = 1\r\nNOEOL = 2"
    f.write_bytes(original)
    monkeypatch.chdir(tmp_path)

    saved = _apply_overlay({"mod.py": "MUT\n"})
    _restore_overlay(saved)
    assert f.read_bytes() == original


def test_overlay_new_file_is_created_then_removed(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    new = tmp_path / "brand_new.py"
    assert not new.exists()

    saved = _apply_overlay({"brand_new.py": "x = 1\n"})
    assert new.read_text() == "x = 1\n"
    assert saved == [("brand_new.py", None)]  # None original -> new-file mutant

    _restore_overlay(saved)
    assert not new.exists()  # removed on restore


def test_overlay_multiple_files_roundtrip(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "a.py").write_text("A")
    (tmp_path / "b.py").write_text("B")

    saved = _apply_overlay({"a.py": "A2", "b.py": "B2"})
    assert (tmp_path / "a.py").read_text() == "A2"
    assert (tmp_path / "b.py").read_text() == "B2"

    _restore_overlay(saved)
    assert (tmp_path / "a.py").read_text() == "A"
    assert (tmp_path / "b.py").read_text() == "B"


def test_restore_new_file_tolerates_already_gone(tmp_path, monkeypatch):
    # A child that already unlinked the file must not make restore raise.
    monkeypatch.chdir(tmp_path)
    saved = _apply_overlay({"gone.py": "x"})
    (tmp_path / "gone.py").unlink()
    _restore_overlay(saved)  # contextlib.suppress(OSError) -> no raise


# ── _ServeChildPlugin: counting + terminal event ───────────────────────────


def test_child_counts_call_reports():
    child = _ServeChildPlugin(FakeConn(), req_id=7)
    child.pytest_runtest_logreport(mk_report("setup", "passed"))  # not counted
    child.pytest_runtest_logreport(mk_report("call", "passed"))  # +1
    child.pytest_runtest_logreport(mk_report("teardown", "passed"))  # not counted
    assert child._ran == 1


def test_child_counts_terminal_setup_outcomes():
    # A skip/error at setup has no call phase -> count it there, once.
    child = _ServeChildPlugin(FakeConn(), req_id=1)
    child.pytest_runtest_logreport(mk_report("setup", "skipped"))
    child.pytest_runtest_logreport(mk_report("setup", "error", failed=True))
    assert child._ran == 2


def test_child_sessionfinish_emits_run_done_not_killed():
    conn = FakeConn()
    child = _ServeChildPlugin(conn, req_id=42)
    child.pytest_runtest_logreport(mk_report("call", "passed"))
    child.pytest_sessionfinish(session=None, exitstatus=0)

    assert conn.sent[-1] == (
        "serve_run_done",
        {"req_id": 42, "killed": False, "ran": 1},
    )


def test_child_failure_flips_killed_and_tags_reports():
    conn = FakeConn()
    child = _ServeChildPlugin(conn, req_id=3)
    child.pytest_runtest_logreport(mk_report("call", "failed", failed=True))
    child.pytest_sessionfinish(session=None, exitstatus=1)

    # The report was tagged as serve_report with the request id...
    kinds = [k for k, _ in conn.sent]
    assert "serve_report" in kinds
    tagged = next(p for k, p in conn.sent if k == "serve_report")
    assert tagged["req_id"] == 3
    # ...and the terminal event reports the kill.
    assert conn.sent[-1] == ("serve_run_done", {"req_id": 3, "killed": True, "ran": 1})


# ── ServeDispatchPlugin: template request loop ─────────────────────────────


def test_collection_finish_announces_nodeids():
    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    session = SimpleNamespace(
        items=[SimpleNamespace(nodeid="t.py::a"), SimpleNamespace(nodeid="t.py::b")]
    )
    plugin.pytest_collection_finish(session)
    assert conn.sent == [("serve_ready", {"nodeids": ["t.py::a", "t.py::b"]})]


def test_runtestloop_dispatches_serve_run_then_ends(monkeypatch):
    conn = FakeConn(
        [
            {"kind": "serve_run", "payload": {"req_id": 1, "ids": ["t.py::a"]}},
            {"kind": "end_session", "payload": {}},
        ]
    )
    plugin = ServeDispatchPlugin(conn)
    seen: list[Any] = []
    monkeypatch.setattr(plugin, "_forked_run", lambda payload: seen.append(payload))

    session = SimpleNamespace(config=SimpleNamespace(option=SimpleNamespace(collectonly=False)))
    assert plugin.pytest_runtestloop(session) is True
    assert seen == [{"req_id": 1, "ids": ["t.py::a"]}]


def test_runtestloop_client_vanish_ends_cleanly():
    plugin = ServeDispatchPlugin(FakeConn([]))  # recv_one -> None
    session = SimpleNamespace(config=SimpleNamespace(option=SimpleNamespace(collectonly=False)))
    assert plugin.pytest_runtestloop(session) is True


def test_runtestloop_collectonly_short_circuits():
    conn = FakeConn([{"kind": "serve_run", "payload": {}}])  # must NOT be consumed
    plugin = ServeDispatchPlugin(conn)
    session = SimpleNamespace(config=SimpleNamespace(option=SimpleNamespace(collectonly=True)))
    assert plugin.pytest_runtestloop(session) is True
    assert conn.recv_one() is not None  # loop never ran, message still queued


def test_forked_run_applies_and_restores_overlay(monkeypatch):
    # Parent path (fork -> pid>0): overlay applied before, restored after,
    # regardless of what the child would do.
    calls: list[tuple[str, Any]] = []
    monkeypatch.setattr(
        dispatch, "_apply_overlay", lambda ov: calls.append(("apply", ov)) or ["token"]
    )
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: calls.append(("restore", s)))
    monkeypatch.setattr(os, "fork", lambda: 4321)  # parent branch
    monkeypatch.setattr(os, "waitpid", lambda pid, flags: calls.append(("waitpid", pid)))

    plugin = ServeDispatchPlugin(FakeConn())
    plugin._forked_run({"req_id": 1, "ids": ["t.py::a"], "overlay": {"m.py": "x"}})

    assert calls == [
        ("apply", {"m.py": "x"}),
        ("waitpid", 4321),
        ("restore", ["token"]),
    ]


def test_forked_run_restores_even_if_fork_raises(monkeypatch):
    # The finally must restore the tree even when the fork/child path blows up.
    calls: list[str] = []
    monkeypatch.setattr(dispatch, "_apply_overlay", lambda ov: ["saved"])
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: calls.append("restored"))

    def boom() -> int:
        raise OSError("fork failed")

    monkeypatch.setattr(os, "fork", boom)

    plugin = ServeDispatchPlugin(FakeConn())
    with contextlib.suppress(OSError):
        plugin._forked_run({"req_id": 1, "ids": [], "overlay": {"m.py": "x"}})
    assert calls == ["restored"]
