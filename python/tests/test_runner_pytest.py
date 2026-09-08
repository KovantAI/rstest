"""Unit tests for the session entrypoints and the crash-containment wrapper."""

from __future__ import annotations

from typing import Any

from rstest_worker._internal import runner_pytest
from rstest_worker._internal.dispatch import ItemDispatchPlugin, LazyDispatchPlugin
from rstest_worker._internal.stream import StreamPlugin


class FakeConn:
    def __init__(self) -> None:
        self.sent: list[tuple[str, Any]] = []

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))


def _capture_main(monkeypatch):
    captured: dict[str, Any] = {}
    monkeypatch.setattr(
        runner_pytest.pytest,
        "main",
        lambda args, plugins: captured.update(args=args, plugins=plugins) or 0,
    )
    return captured


def test_run_uses_stream_plugin(monkeypatch):
    captured = _capture_main(monkeypatch)
    assert runner_pytest.run(["t.py"], FakeConn()) == 0
    assert captured["args"] == ["t.py"]
    assert isinstance(captured["plugins"][0], StreamPlugin)


def test_run_session_uses_item_dispatch_plugin(monkeypatch):
    captured = _capture_main(monkeypatch)
    runner_pytest.run_session(["t.py"], FakeConn())
    assert isinstance(captured["plugins"][0], ItemDispatchPlugin)


def test_run_lazy_session_uses_lazy_dispatch_plugin(monkeypatch):
    captured = _capture_main(monkeypatch)
    runner_pytest.run_lazy_session(["t.py"], FakeConn())
    assert isinstance(captured["plugins"][0], LazyDispatchPlugin)


def test_run_copies_args_not_aliases(monkeypatch):
    captured = _capture_main(monkeypatch)
    original = ["t.py"]
    runner_pytest.run(original, FakeConn())
    assert captured["args"] == original
    assert captured["args"] is not original  # list(args) defensively copied


def test_contained_returns_session_exit_status():
    assert runner_pytest._contained(lambda: 3, FakeConn()) == 3


def test_prime_coverage_core_sets_ctrace_for_cov_context(monkeypatch):
    monkeypatch.delenv("COVERAGE_CORE", raising=False)
    runner_pytest._prime_coverage_core(["--cov-context=test"])
    import os

    assert os.environ["COVERAGE_CORE"] == "ctrace"


def test_prime_coverage_core_respects_user_choice(monkeypatch):
    monkeypatch.setenv("COVERAGE_CORE", "sysmon")
    runner_pytest._prime_coverage_core(["--cov-context=test"])
    import os

    assert os.environ["COVERAGE_CORE"] == "sysmon"


def test_prime_coverage_core_noop_without_cov_context(monkeypatch):
    monkeypatch.delenv("COVERAGE_CORE", raising=False)
    runner_pytest._prime_coverage_core(["t.py"])
    import os

    assert "COVERAGE_CORE" not in os.environ


def test_contained_maps_keyboard_interrupt_to_2():
    # pytest's Interrupted subclasses KeyboardInterrupt -> exit 2, no crash.
    def raise_interrupt():
        raise KeyboardInterrupt

    conn = FakeConn()
    assert runner_pytest._contained(raise_interrupt, conn) == 2
    assert conn.sent == []  # errors already reported via collectreport


def test_contained_reports_base_exception_as_collect_error():
    # A config-time Skipped (importorskip) escaping pytest.main must be caught,
    # reported once, and downgraded to exit 1 instead of killing the worker.
    def raise_skipped():
        raise BaseException("config-time boom")

    conn = FakeConn()
    assert runner_pytest._contained(raise_skipped, conn) == 1
    assert len(conn.sent) == 1
    kind, payload = conn.sent[0]
    assert kind == "collect_error"
    assert payload["path"] == "<session: BaseException>"
    assert "config-time boom" in payload["longrepr"]
