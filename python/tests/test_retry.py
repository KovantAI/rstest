"""Flaky-mark budgets, single-session flaky reruns, and the -x / --maxfail
exemptions (quarantine, retryable attempts)."""

from __future__ import annotations

import uuid
from types import SimpleNamespace

import pytest
from rstest_worker._internal import runner_pytest
from rstest_worker._internal.retry import FlakyReruns, MaxfailExemptions, flaky_reruns


def _item(*args, **kwargs):
    mark = SimpleNamespace(args=args, kwargs=kwargs)
    return SimpleNamespace(
        get_closest_marker=lambda name: mark if name == "flaky" else None,
        config=SimpleNamespace(),
        obj=lambda: None,
    )


def test_flaky_reruns_without_mark_is_zero():
    item = SimpleNamespace(get_closest_marker=lambda name: None)
    assert flaky_reruns(item) == 0


@pytest.mark.parametrize(
    ("args", "kwargs", "expected"),
    [
        ((), {}, 1),
        ((3,), {}, 3),
        ((), {"reruns": 2}, 2),
        ((5,), {"reruns": 2}, 2),
        ((), {"reruns": 0}, 0),
        ((), {"reruns": "x"}, 0),
        ((), {"reruns": 2, "condition": False}, 0),
        ((), {"reruns": 2, "condition": True}, 2),
        ((), {"reruns": 2, "condition": "sys.platform == 'no-such-os'"}, 0),
        ((), {"reruns": 2, "condition": "os.sep in ('/', '\\\\')"}, 2),
        # Unevaluable condition: rstest keeps the reruns.
        ((), {"reruns": 2, "condition": "undefined_name"}, 2),
    ],
)
def test_flaky_reruns_reads_the_mark_like_rerunfailures(args, kwargs, expected):
    assert flaky_reruns(_item(*args, **kwargs)) == expected


def _session(tmp_path, monkeypatch, source, *args, plugins, quarantine=None):
    """Run a real in-process session over one test file; returns
    (exit status, outcomes by (nodeid, when))."""
    if quarantine is not None:
        monkeypatch.setenv("RSTEST_QUARANTINE", quarantine)
    else:
        monkeypatch.delenv("RSTEST_QUARANTINE", raising=False)
    name = f"test_retry_{uuid.uuid4().hex[:8]}.py"
    (tmp_path / name).write_text(source)
    seen: list[tuple[str, str, str]] = []

    class Recorder:
        def pytest_runtest_logreport(self, report):
            seen.append((report.nodeid.split("::", 1)[1], report.when, report.outcome))

    status = runner_pytest._pytest_main(
        [str(tmp_path / name), "-q", "-p", "no:cacheprovider", "-p", "no:randomly", *args],
        plugins=[*plugins(), Recorder()],
    )
    return int(status), seen


FLAKY_SRC = (
    "import pytest\n"
    "_n = {'a': 0}\n"
    "@pytest.mark.flaky(reruns=2)\n"
    "def test_recovers():\n"
    "    _n['a'] += 1\n"
    "    assert _n['a'] >= 3\n"
    "@pytest.mark.flaky(1)\n"
    "def test_always():\n"
    "    assert False\n"
    "def test_ok():\n"
    "    pass\n"
)


def test_session_reruns_flaky_marks(tmp_path, monkeypatch):
    status, seen = _session(tmp_path, monkeypatch, FLAKY_SRC, plugins=lambda: [FlakyReruns()])
    calls = [(n, o) for n, w, o in seen if w == "call"]
    assert calls == [
        ("test_recovers", "rerun"),
        ("test_recovers", "rerun"),
        ("test_recovers", "passed"),
        ("test_always", "rerun"),
        ("test_always", "failed"),
        ("test_ok", "passed"),
    ]
    assert status == 1  # test_always exhausted its budget


def test_rerun_attempts_do_not_trip_x(tmp_path, monkeypatch):
    # The first failed attempt is a `rerun`, not a failure: -x waits for the
    # last attempt, so test_recovers passes and test_always stops the run.
    status, seen = _session(tmp_path, monkeypatch, FLAKY_SRC, "-x", plugins=lambda: [FlakyReruns()])
    calls = [(n, o) for n, w, o in seen if w == "call"]
    assert ("test_recovers", "passed") in calls
    assert ("test_always", "failed") in calls
    assert ("test_ok", "passed") not in calls
    assert status == 1


def test_rerun_redoes_a_failed_module_setup(tmp_path, monkeypatch):
    src = (
        "import pytest\n"
        "_n = {'a': 0}\n"
        "@pytest.fixture(scope='module')\n"
        "def res():\n"
        "    _n['a'] += 1\n"
        "    assert _n['a'] >= 2, 'first setup fails'\n"
        "    return _n['a']\n"
        "@pytest.mark.flaky(reruns=1)\n"
        "def test_uses(res):\n"
        "    assert res == 2\n"
    )
    status, seen = _session(tmp_path, monkeypatch, src, plugins=lambda: [FlakyReruns()])
    assert ("test_uses", "setup", "rerun") in seen
    assert ("test_uses", "call", "passed") in seen
    assert status == 0


QUAR_SRC = (
    "def test_a_quarantined():\n    assert False\n"
    "def test_b_ok():\n    pass\n"
    "def test_c_real():\n    assert False\n"
    "def test_d_ok():\n    pass\n"
)


def test_quarantined_failure_does_not_trip_x(tmp_path, monkeypatch):
    status, seen = _session(
        tmp_path,
        monkeypatch,
        QUAR_SRC,
        "-x",
        plugins=lambda: [MaxfailExemptions(pool=False)],
        quarantine=r"^.*::test_a_quarantined$",
    )
    calls = [(n, o) for n, w, o in seen if w == "call"]
    assert calls == [
        ("test_a_quarantined", "failed"),
        ("test_b_ok", "passed"),
        ("test_c_real", "failed"),
    ]
    assert status == 1


def test_quarantine_exemption_keeps_maxfail_exact(tmp_path, monkeypatch):
    # --maxfail=2 with one quarantined failure: the run goes on to the second
    # real failure instead of stopping at the first.
    src = QUAR_SRC + "def test_e_real():\n    assert False\ndef test_f_ok():\n    pass\n"
    status, seen = _session(
        tmp_path,
        monkeypatch,
        src,
        "--maxfail=2",
        plugins=lambda: [MaxfailExemptions(pool=False)],
        quarantine=r"^.*::test_a_quarantined$",
    )
    calls = [n for n, w, _ in seen if w == "call"]
    assert calls[-1] == "test_e_real"
    assert status == 1


def test_without_maxfail_the_count_is_untouched(tmp_path, monkeypatch):
    # No -x: the quarantined failure still counts in pytest's own exit status
    # (rstest demotes it after the run).
    status, _ = _session(
        tmp_path,
        monkeypatch,
        "def test_a_quarantined():\n    assert False\n",
        plugins=lambda: [MaxfailExemptions(pool=False)],
        quarantine=r"^.*::test_a_quarantined$",
    )
    assert status == 1


def test_pool_exempts_retryable_attempts(tmp_path, monkeypatch):
    # In a pool worker the orchestrator owns retries: with --reruns in play
    # no failed attempt may stop the worker's own session.
    monkeypatch.setenv("RSTEST_RERUNS", "1")
    status, seen = _session(
        tmp_path, monkeypatch, QUAR_SRC, "-x", plugins=lambda: [MaxfailExemptions(pool=True)]
    )
    assert [n for n, w, _ in seen if w == "call"][-1] == "test_d_ok"
    assert status == 0


def test_pool_exempts_flaky_marked_attempts_only(tmp_path, monkeypatch):
    monkeypatch.delenv("RSTEST_RERUNS", raising=False)
    src = (
        "import pytest\n"
        "@pytest.mark.flaky(reruns=1)\n"
        "def test_a_marked():\n    assert False\n"
        "def test_b_ok():\n    pass\n"
        "def test_c_real():\n    assert False\n"
        "def test_d_ok():\n    pass\n"
    )
    status, seen = _session(
        tmp_path, monkeypatch, src, "-x", plugins=lambda: [MaxfailExemptions(pool=True)]
    )
    assert [n for n, w, _ in seen if w == "call"][-1] == "test_c_real"
    assert status == 1


def test_exemptions_inactive_without_pool_or_quarantine(monkeypatch):
    monkeypatch.delenv("RSTEST_QUARANTINE", raising=False)
    assert not MaxfailExemptions(pool=False).active()
    assert MaxfailExemptions(pool=True).active()
    monkeypatch.setenv("RSTEST_QUARANTINE", "^a$\n(unclosed\n")
    exemptions = MaxfailExemptions(pool=False)
    assert exemptions.active()
    assert len(exemptions._quarantine) == 1  # the bad pattern is skipped


def test_flaky_reruns_steps_aside_for_rerunfailures():
    plugin = FlakyReruns()
    unregistered = []
    manager = SimpleNamespace(
        hasplugin=lambda name: name == "rerunfailures",
        unregister=unregistered.append,
    )
    plugin.pytest_configure(SimpleNamespace(pluginmanager=manager))
    assert unregistered == [plugin]


def test_rerun_teststatus():
    plugin = FlakyReruns()
    assert plugin.pytest_report_teststatus(SimpleNamespace(outcome="rerun"))[1] == "R"
    assert plugin.pytest_report_teststatus(SimpleNamespace(outcome="passed")) is None
