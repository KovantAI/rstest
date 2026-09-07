"""Unit tests for serve-mode dispatch: overlay lifecycle, the forked-child
report/count plumbing, and the template plugin's request loop. The fork itself
is exercised end-to-end by e2e/gate.py; here we test the pure seams around it."""

from __future__ import annotations

import contextlib
import os
import sys
from types import SimpleNamespace
from typing import Any

import pytest
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


def test_apply_overlay_rolls_back_on_mid_batch_failure(tmp_path, monkeypatch):
    # If a later write in the batch fails, files already written must be
    # reverted (not left mutated on disk) before the error propagates.
    monkeypatch.chdir(tmp_path)
    (tmp_path / "a.py").write_text("ORIG")
    (tmp_path / "d").mkdir()  # opening a directory path for write raises OSError

    with pytest.raises(OSError):
        _apply_overlay({"a.py": "MUTATED", "d": "boom"})  # dict order: a.py, then d

    assert (tmp_path / "a.py").read_text() == "ORIG"  # rolled back, not "MUTATED"


def test_overlay_writes_atomically_and_leaves_no_temp_files(tmp_path, monkeypatch):
    # The overlay must land via tmp + os.replace (never a truncating in-place
    # open), so a crash mid-write can't corrupt the user's real source. Verify
    # the discipline's observable trace: the temp sidecar is always cleaned up,
    # on both the success and the rollback paths.
    monkeypatch.chdir(tmp_path)
    (tmp_path / "a.py").write_text("ORIG")

    def temp_sidecars():
        return [p.name for p in tmp_path.iterdir() if p.name.startswith(".rstest-overlay-")]

    saved = _apply_overlay({"a.py": "MUT"})
    assert (tmp_path / "a.py").read_text() == "MUT"
    assert temp_sidecars() == []  # replace consumed the tmp; nothing leaked
    _restore_overlay(saved)
    assert (tmp_path / "a.py").read_text() == "ORIG"
    assert temp_sidecars() == []

    # Rollback path (a later write fails) must not strand a temp file either.
    (tmp_path / "d").mkdir()
    with pytest.raises(OSError):
        _apply_overlay({"a.py": "X", "d": "boom"})
    assert temp_sidecars() == []


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
    # status 0 == child exited cleanly (it sent serve_run_done itself).
    monkeypatch.setattr(
        os, "waitpid", lambda pid, flags: calls.append(("waitpid", pid)) or (pid, 0)
    )

    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    plugin._forked_run({"req_id": 1, "ids": ["t.py::a"], "overlay": {"m.py": "x"}})

    assert calls == [
        ("apply", {"m.py": "x"}),
        ("waitpid", 4321),
        ("restore", ["token"]),
    ]
    # Clean child exit -> the parent must NOT emit a duplicate terminal event.
    assert conn.sent == []


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


def test_forked_run_parent_emits_run_done_when_child_dies(monkeypatch):
    # Child that crashed before sending serve_run_done (nonzero exit / signal):
    # the parent must emit a terminal event so the client never blocks, and a
    # crashed mutant counts as killed.
    monkeypatch.setattr(dispatch, "_apply_overlay", lambda ov: [])
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: None)
    monkeypatch.setattr(os, "fork", lambda: 4321)  # parent branch
    monkeypatch.setattr(os, "waitpid", lambda pid, flags: (pid, 1 << 8))  # exit code 1

    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    plugin._forked_run({"req_id": 77, "ids": ["t.py::a"], "overlay": {}})

    assert conn.sent == [("serve_run_done", {"req_id": 77, "killed": True, "ran": 0})]


def test_forked_run_parent_emits_run_done_when_child_signaled(monkeypatch):
    # A child killed by a signal (e.g. a segfaulting C-extension mutant): status
    # has no WIFEXITED bit, so it's abnormal -> parent emits killed.
    monkeypatch.setattr(dispatch, "_apply_overlay", lambda ov: [])
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: None)
    monkeypatch.setattr(os, "fork", lambda: 4321)
    monkeypatch.setattr(os, "waitpid", lambda pid, flags: (pid, 9))  # killed by SIGKILL

    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    plugin._forked_run({"req_id": 3, "ids": [], "overlay": {}})

    assert conn.sent == [("serve_run_done", {"req_id": 3, "killed": True, "ran": 0})]


def test_forked_run_reports_when_overlay_apply_fails(monkeypatch):
    # An overlay that can't be written (rolled back inside _apply_overlay) must
    # not fork or block the client: report the run finished, killed=False (infra
    # error, not a caught mutant).
    def boom(_overlay):
        raise OSError("disk full")

    monkeypatch.setattr(dispatch, "_apply_overlay", boom)
    forked: list[int] = []
    monkeypatch.setattr(os, "fork", lambda: forked.append(1))

    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    plugin._forked_run({"req_id": 9, "ids": ["t.py::a"], "overlay": {"m.py": "x"}})

    assert conn.sent == [("serve_run_done", {"req_id": 9, "killed": False, "ran": 0})]
    assert forked == []  # never forked when the overlay couldn't be applied


def test_forked_run_survives_non_oserror_overlay_failure(monkeypatch):
    # Regression: a malformed overlay value (e.g. non-str content whose `.encode`
    # raises AttributeError) must NOT escape _forked_run and kill the whole serve
    # session (dropping every queued request). It's an infra error, not a caught
    # mutant: report the run finished, killed=False, and never fork.
    def boom(_overlay):
        raise AttributeError("'int' object has no attribute 'encode'")

    monkeypatch.setattr(dispatch, "_apply_overlay", boom)
    forked: list[int] = []
    monkeypatch.setattr(os, "fork", lambda: forked.append(1))

    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    plugin._forked_run({"req_id": 5, "ids": ["t.py::a"], "overlay": {"m.py": 123}})

    assert conn.sent == [("serve_run_done", {"req_id": 5, "killed": False, "ran": 0})]
    assert forked == []


def test_forked_run_survives_fork_exhaustion(monkeypatch):
    # Regression: os.fork() raising OSError (EAGAIN/ENOMEM under load) must not
    # crash the session. The overlay is applied then restored, no terminal event
    # is lost (client gets serve_run_done), and it does not propagate.
    calls: list[str] = []
    monkeypatch.setattr(dispatch, "_apply_overlay", lambda ov: ["saved"])
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: calls.append("restored"))

    def eagain() -> int:
        raise OSError("Resource temporarily unavailable")

    monkeypatch.setattr(os, "fork", eagain)

    conn = FakeConn()
    plugin = ServeDispatchPlugin(conn)
    plugin._forked_run({"req_id": 8, "ids": ["t.py::a"], "overlay": {"m.py": "x"}})

    assert conn.sent == [("serve_run_done", {"req_id": 8, "killed": False, "ran": 0})]
    assert calls == ["restored"]  # overlay still rolled back via finally


def test_child_run_disables_bytecode_and_drops_stale_pyc(tmp_path, monkeypatch):
    # Regression: a forked child must not read a stale .pyc for an overlaid
    # source. Many mutation operators preserve byte size and consecutive fast
    # forks land in the same wall-clock second, so a cached pyc whose
    # (mtime-in-seconds, size) still matches would be loaded in preference to the
    # freshly-overlaid bytes -> a killed mutant silently misreported as survivor.
    # Assert the child disables bytecode writes and removes cached pyc for the
    # overlaid file.
    import importlib.util

    monkeypatch.chdir(tmp_path)
    src = tmp_path / "mod.py"
    src.write_text("VAL = 1\n")
    pyc = importlib.util.cache_from_source(str(src.resolve()))
    os.makedirs(os.path.dirname(pyc), exist_ok=True)
    with open(pyc, "wb") as fh:
        fh.write(b"stale-bytecode")
    assert os.path.exists(pyc)

    # Auto-restored after the test; the child sets it True as a side effect.
    monkeypatch.setattr(sys, "dont_write_bytecode", False, raising=False)
    # Skip the real pytest run; we only assert the cache-invalidation seam.
    monkeypatch.setattr(dispatch.pytest, "main", lambda *a, **k: 0)

    plugin = ServeDispatchPlugin(FakeConn())
    plugin._baseline = set(sys.modules)  # nothing to drop from sys.modules
    plugin._child_run(1, ["mod.py::x"], False, overlay={"mod.py": "VAL = 2\n"})

    assert sys.dont_write_bytecode is True
    assert not os.path.exists(pyc), "stale pyc for an overlaid source must be removed"


# ── collection / import failure counts as killed (spec: killed = failed OR
#    errored) ──────────────────────────────────────────────────────────────


def mk_collectreport(*, failed: bool, skipped: bool = False) -> SimpleNamespace:
    return SimpleNamespace(
        nodeid="mod.py",
        failed=failed,
        skipped=skipped,
        longreprtext="ImportError: boom",
    )


def test_child_collect_error_marks_run_killed():
    # A mutant that breaks import fails collection; the child's serve_run_done
    # must report killed=True even though no test call ever ran.
    conn = FakeConn()
    child = _ServeChildPlugin(conn, req_id=8)
    child.pytest_collectreport(mk_collectreport(failed=True))
    child.pytest_sessionfinish(session=None, exitstatus=2)

    assert ("collect_error", {"path": "mod.py", "longrepr": "ImportError: boom"}) in conn.sent
    assert conn.sent[-1] == ("serve_run_done", {"req_id": 8, "killed": True, "ran": 0})


def test_child_internalerror_marks_run_killed():
    conn = FakeConn()
    child = _ServeChildPlugin(conn, req_id=4)
    child.pytest_internalerror("boom")
    child.pytest_sessionfinish(session=None, exitstatus=3)

    assert conn.sent[-1] == ("serve_run_done", {"req_id": 4, "killed": True, "ran": 0})


def test_collect_skip_does_not_mark_killed():
    # A skipped collector (importorskip) is NOT a kill.
    conn = FakeConn()
    child = _ServeChildPlugin(conn, req_id=1)
    child.pytest_collectreport(mk_collectreport(failed=False, skipped=True))
    child.pytest_sessionfinish(session=None, exitstatus=0)

    assert conn.sent[-1] == ("serve_run_done", {"req_id": 1, "killed": False, "ran": 0})


# ── pytest_collection: framework baseline snapshot ─────────────────────────


def test_collection_snapshots_framework_baseline():
    # The wrapper hook records sys.modules before test modules are imported,
    # then yields to the real collection and passes its result through.
    plugin = ServeDispatchPlugin(FakeConn())
    gen = plugin.pytest_collection(session=SimpleNamespace())
    assert next(gen) is None  # wrapper yields to the inner hook
    assert isinstance(plugin._baseline, set) and "sys" in plugin._baseline
    with pytest.raises(StopIteration) as stop:
        gen.send("collected")  # resume; wrapper returns the inner result
    assert stop.value.value == "collected"


# ── _child_run: module reset + subset session ──────────────────────────────


class _Exit(BaseException):
    """Stand-in for the process-ending os._exit so a test can observe its code
    instead of the interpreter vanishing."""

    def __init__(self, code: int) -> None:
        self.code = code


def test_child_run_resets_modules_and_runs_requested_ids(monkeypatch):
    captured: dict[str, Any] = {}
    monkeypatch.setattr(
        pytest, "main", lambda args, plugins: captured.update(args=args, plugins=plugins) or 0
    )
    plugin = ServeDispatchPlugin(FakeConn())
    plugin._baseline = set(sys.modules)
    # A module imported *after* the baseline must be dropped so the child
    # re-imports it fresh (seeing the overlay).
    monkeypatch.setitem(sys.modules, "_serve_fake_sut", SimpleNamespace())

    plugin._child_run(req_id=9, ids=["t.py::a", "t.py::b"], stop=False)

    assert "_serve_fake_sut" not in sys.modules  # reset dropped it
    assert captured["args"][:2] == ["t.py::a", "t.py::b"]
    assert "-p" in captured["args"] and "no:cacheprovider" in captured["args"]
    assert "-x" not in captured["args"]
    child = captured["plugins"][0]
    assert isinstance(child, _ServeChildPlugin) and child._serve_req_id == 9


def test_child_run_drops_overlaid_baseline_module(monkeypatch, tmp_path):
    # A module imported during config/collection (e.g. a conftest) lives IN the
    # baseline, so the baseline reset would keep it — but if the overlay mutates
    # its source file, the child must re-import the mutation, not the stale
    # module. The overlaid file's module is dropped even though it's baseline.
    conftest = tmp_path / "conftest.py"
    conftest.write_text("X = 1\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(pytest, "main", lambda args, plugins: 0)

    fake = SimpleNamespace(__file__=str(conftest))
    monkeypatch.setitem(sys.modules, "_serve_fake_conftest", fake)

    plugin = ServeDispatchPlugin(FakeConn())
    plugin._baseline = set(sys.modules)  # conftest module is IN the baseline now

    # Overlay names the same file by its relative path (as a client sends it).
    plugin._child_run(req_id=1, ids=["t.py::a"], stop=False, overlay={"conftest.py": "X = 2\n"})

    assert "_serve_fake_conftest" not in sys.modules  # dropped -> re-imports mutation


def test_child_run_keeps_baseline_module_when_not_overlaid(monkeypatch, tmp_path):
    # The overlaid-file drop must be surgical: a baseline module NOT in the
    # overlay stays put (dropping all baseline modules would nuke the framework).
    other = tmp_path / "helper.py"
    other.write_text("Y = 1\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(pytest, "main", lambda args, plugins: 0)

    fake = SimpleNamespace(__file__=str(other))
    monkeypatch.setitem(sys.modules, "_serve_untouched", fake)

    plugin = ServeDispatchPlugin(FakeConn())
    plugin._baseline = set(sys.modules)

    plugin._child_run(req_id=1, ids=["t.py::a"], stop=False, overlay={"conftest.py": "X = 2\n"})

    assert "_serve_untouched" in sys.modules  # not overlaid -> retained


def test_child_run_same_basename_different_dir_is_not_dropped(monkeypatch, tmp_path):
    # The cheap basename prefilter must not over-match: a baseline module that
    # shares a name with an overlaid file but lives in a different directory
    # (its abspath differs) stays loaded — only the actually-overlaid file goes.
    (tmp_path / "pkg").mkdir()
    other_conftest = tmp_path / "pkg" / "conftest.py"  # same basename, other dir
    other_conftest.write_text("Z = 1\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(pytest, "main", lambda args, plugins: 0)

    fake = SimpleNamespace(__file__=str(other_conftest))
    monkeypatch.setitem(sys.modules, "_serve_other_conftest", fake)

    plugin = ServeDispatchPlugin(FakeConn())
    plugin._baseline = set(sys.modules)

    # Overlay targets ./conftest.py, NOT ./pkg/conftest.py.
    plugin._child_run(req_id=1, ids=["t.py::a"], stop=False, overlay={"conftest.py": "X = 2\n"})

    assert "_serve_other_conftest" in sys.modules  # basename collided, abspath didn't


def test_child_run_stop_prepends_dash_x(monkeypatch):
    captured: dict[str, Any] = {}
    monkeypatch.setattr(
        pytest, "main", lambda args, plugins: captured.setdefault("args", args) or 0
    )
    plugin = ServeDispatchPlugin(FakeConn())
    plugin._baseline = set(sys.modules)

    plugin._child_run(req_id=1, ids=["t.py::a"], stop=True)

    assert captured["args"][0] == "-x"  # stop_on_first_fail bails after first fail


# ── _forked_run: the child (pid == 0) branch ───────────────────────────────


def test_forked_run_child_branch_exits_zero_on_success(monkeypatch):
    monkeypatch.setattr(dispatch, "_apply_overlay", lambda ov: ["saved"])
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: None)
    monkeypatch.setattr(os, "fork", lambda: 0)  # child branch
    monkeypatch.setattr(os, "_exit", lambda code: (_ for _ in ()).throw(_Exit(code)))
    seen: dict[str, Any] = {}
    monkeypatch.setattr(
        ServeDispatchPlugin,
        "_child_run",
        lambda self, r, i, s, ov: seen.update(r=r, i=i, s=s, ov=ov),
    )

    plugin = ServeDispatchPlugin(FakeConn())
    with pytest.raises(_Exit) as ei:
        plugin._forked_run(
            {"req_id": 5, "ids": ["t.py::a"], "overlay": {}, "stop_on_first_fail": False}
        )
    assert ei.value.code == 0
    assert seen == {"r": 5, "i": ["t.py::a"], "s": False, "ov": {}}


def test_forked_run_child_branch_exits_one_when_run_raises(monkeypatch):
    monkeypatch.setattr(dispatch, "_apply_overlay", lambda ov: [])
    monkeypatch.setattr(dispatch, "_restore_overlay", lambda s: None)
    monkeypatch.setattr(os, "fork", lambda: 0)  # child branch
    monkeypatch.setattr(os, "_exit", lambda code: (_ for _ in ()).throw(_Exit(code)))

    def boom(self, r, i, s, ov):
        raise RuntimeError("child session blew up")

    monkeypatch.setattr(ServeDispatchPlugin, "_child_run", boom)

    plugin = ServeDispatchPlugin(FakeConn())
    with pytest.raises(_Exit) as ei:
        plugin._forked_run({"req_id": 1, "ids": [], "overlay": {}})
    assert ei.value.code == 1  # BaseException in child -> nonzero exit


# ── runner_pytest / __main__ wiring ────────────────────────────────────────


def test_run_serve_session_runs_pytest_with_serve_plugin(monkeypatch):
    from rstest_worker._internal import runner_pytest

    captured: dict[str, Any] = {}
    monkeypatch.setattr(
        runner_pytest.pytest,
        "main",
        lambda args, plugins: captured.update(args=args, plugins=plugins) or 0,
    )
    rc = runner_pytest.run_serve_session(["t.py"], FakeConn())
    assert rc == 0
    assert captured["args"] == ["t.py"]
    assert isinstance(captured["plugins"][0], ServeDispatchPlugin)


class _MainConn:
    """A conn shaped for __main__._serve: yields queued commands, records sends."""

    def __init__(self, cmds: list[dict[str, Any]]) -> None:
        self._cmds = cmds
        self.sent: list[tuple[str, Any]] = []

    def commands(self):
        return iter(self._cmds)

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))


def test_serve_loop_dispatches_run_serve_session(monkeypatch):
    from rstest_worker import __main__ as main_mod

    monkeypatch.setattr(
        main_mod.runner_pytest,
        "run_serve_session",
        lambda args, conn: 7 if args == ["t.py"] else -1,
    )
    conn = _MainConn(
        [
            {"kind": "run_serve_session", "payload": {"args": ["t.py"]}},
            {"kind": "shutdown", "payload": {}},
        ]
    )
    main_mod._serve(conn)
    assert ("done", {"exitstatus": 7}) in conn.sent
