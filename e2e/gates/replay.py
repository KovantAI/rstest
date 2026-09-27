"""e2e gate sections: `rstest replay` (record a parallel schedule, re-pin it)."""

import json
import shutil
import tempfile
from pathlib import Path

from _harness import (
    REPLAY_CRASH,
    REPLAY_ORDER,
    SERIAL,
    check,
    clear_e2e_log,
    read_e2e_rows,
)


def _project(files):
    """A fresh project dir OUTSIDE g.tmp: an ini another section left in g.tmp
    would move the rootdir and change the nodeids the journals below name.
    Resolved: Windows temp dirs come back as 8.3 short names (RUNNER~1), and
    pytest's rootdir is the common ancestor of the cwd and the args, so a short
    cwd plus a long absolute arg would root the nodeids at C:\\Users."""
    d = Path(tempfile.mkdtemp(prefix="rstest-gate-replay-")).resolve()
    for rel, content in files.items():
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
    return d


def _journal(d, name, assignment, args=()):
    """Hand-written journal, so the schedule under test is fixed, not emergent."""
    (d / name).write_text(
        json.dumps(
            {
                "schema": 1,
                "rstest_version": "gate",
                "run_uid": name,
                "created_epoch": 0,
                "workers": len(assignment),
                "dist": "load",
                "shuffle_seed": None,
                "args": list(args),
                "collection_hash": None,
                "collection_size": sum(len(w) for w in assignment),
                "assignment": assignment,
            }
        ),
        encoding="utf-8",
    )


def _replay(g, d, journal, log, env=None):
    clear_e2e_log(log)
    r = g.run(
        "replay",
        "--journal",
        journal,
        cwd=str(d),
        env_extra={"RSTEST_E2E_LOG": str(log), **(env or {})},
    )
    return r, read_e2e_rows(log)


def _ran(rows, worker):
    return [x["name"] for x in sorted(rows, key=lambda x: x["start"]) if x["worker"] == worker]


def gate_replay(g, args, binary):
    print("== replay: record + pin ==")
    d = _project({"test_order.py": REPLAY_ORDER})
    log = d / "e2e.jsonl"
    p, v, o, s = (f"test_order.py::test_{n}" for n in ("polluter", "victim", "other", "slow"))

    # Record: every pool run journals its schedule, args stored portably (an
    # absolute path under the cwd comes back relative).
    clear_e2e_log(log)
    r = g.run(
        "-n",
        "2",
        str(d.resolve() / "test_order.py"),
        "-k",
        "not always_fails",
        cwd=str(d),
        env_extra={"RSTEST_E2E_LOG": str(log)},
    )
    latest = d / ".rstest_cache" / "replay" / "latest.json"
    check("replay: pool run writes latest.json", latest.exists(), r.stdout[-200:])
    if latest.exists():
        j = json.loads(latest.read_text(encoding="utf-8"))
        check(
            "replay: absolute arg recorded relative",
            j["args"] == ["test_order.py", "-k", "not always_fails"],
            str(j["args"]),
        )
        check(
            "replay: every started test journaled once",
            sorted(t for w in j["assignment"] for t in w) == sorted([p, v, o, s]),
            str(j["assignment"]),
        )
        r2 = g.run("replay", cwd=str(d))
        check(
            "replay: banner lists the recorded args",
            "args: test_order.py -k 'not always_fails'" in r2.stderr + r2.stdout,
            (r2.stderr + r2.stdout)[-300:],
        )

    # The point of replay: the order-dependent failure follows the schedule.
    _journal(d, "together.json", [[p, v], [o]])
    _journal(d, "apart.json", [[p, o], [v]])
    fails = passes = 0
    for _ in range(3):
        r, _rows = _replay(g, d, "together.json", log)
        fails += r.returncode == 1 and "[gw0] test_order.py::test_victim" in r.stdout
        r, _rows = _replay(g, d, "apart.json", log)
        passes += r.returncode == 0
    check("replay: polluter-then-victim fails 3/3", fails == 3, r.stdout[-300:])
    check("replay: split schedule passes 3/3", passes == 3, r.stdout[-300:])

    # gw0 drains first while gw1 is busy: each worker runs exactly its pinned
    # list once (the id-carrier fallback used to hand the whole suite out again).
    _journal(d, "pinned.json", [[o], [v, s, p]])
    r, rows = _replay(g, d, "pinned.json", log)
    check("replay: pinned run green", r.returncode == 0, r.stdout[-300:])
    check("replay: gw0 ran only its list", _ran(rows, "gw0") == ["other"], str(rows))
    check(
        "replay: gw1 ran its list in order",
        _ran(rows, "gw1") == ["victim", "slow", "polluter"],
        str(rows),
    )
    check("replay: no test ran twice", len(rows) == 4, str(rows))
    check("replay: no fallback queue", "id-carrier" not in r.stdout + r.stderr)

    # Reruns are off under replay: @mark.flaky AND [tool.rstest] reruns. A
    # retried attempt had no queue to go to and the failure vanished.
    (d / "pyproject.toml").write_text("[tool.rstest]\nreruns = 2\n", encoding="utf-8")
    _journal(d, "flaky.json", [["test_order.py::test_always_fails"], [o]])
    r, rows = _replay(g, d, "flaky.json", log, env={"RSTEST_REPLAY_FAIL": "1"})
    check("replay: reruns off, failure kept", "1 failed, 1 passed" in r.stdout, r.stdout[-300:])
    check("replay: failing test ran once", _ran(rows, "gw0") == ["always_fails"], str(rows))
    (d / "pyproject.toml").unlink()

    # A recorded --lf must not re-filter by this machine's pytest cache.
    cache = d / ".pytest_cache" / "v" / "cache"
    cache.mkdir(parents=True, exist_ok=True)
    (cache / "lastfailed").write_text(json.dumps({o: True}), encoding="utf-8")
    _journal(d, "lf.json", [[p, v], [o]], args=["--lf"])
    r, rows = _replay(g, d, "lf.json", log)
    check("replay: recorded --lf dropped", "ignoring recorded --lf" in r.stderr + r.stdout)
    check("replay: --lf journal still reproduces", r.returncode == 1 and len(rows) == 3, str(rows))

    # Portability: the same project at another path (another machine) replays
    # the recorded journal unchanged.
    if latest.exists():
        moved = Path(tempfile.mkdtemp(prefix="rstest-gate-replay-moved-")).resolve()
        shutil.copy(d / "test_order.py", moved / "test_order.py")
        shutil.copy(latest, moved / "ci.json")
        mlog = moved / "e2e.jsonl"
        r, rows = _replay(g, moved, "ci.json", mlog)
        check(
            "replay: journal portable across checkouts",
            r.returncode in (0, 1) and len(rows) == 4,
            r.stdout[-300:],
        )
        shutil.rmtree(moved, ignore_errors=True)
    shutil.rmtree(d, ignore_errors=True)


def gate_replay_serial(g, args, binary):
    print("== replay: serial tests stay exclusive ==")
    d = _project({"test_serial.py": SERIAL})
    log = d / "e2e.jsonl"
    t = "test_serial.py::test_"
    # Serial tests listed FIRST on gw0: replay must still hold them until every
    # other worker has finished, as the recording's serial phase did.
    _journal(
        d,
        "serial.json",
        [
            [t + "serial_one", t + "par_a", t + "serial_two", t + "par_b"],
            [t + "par_c", t + "par_d"],
            [t + "par_e", t + "par_f"],
        ],
    )
    r, rows = _replay(g, d, "serial.json", log)
    check("replay serial: green", "8 passed" in r.stdout, r.stdout[-200:])
    serial = [x for x in rows if x["name"].startswith("serial")]
    par = [x for x in rows if x["name"].startswith("par")]
    overlap = any(
        s["start"] < o["end"] and o["start"] < s["end"] for s in serial for o in rows if o is not s
    )
    check("replay serial: exclusive", not overlap and len(serial) == 2, str(rows))
    check(
        "replay serial: after parallel",
        bool(serial) and min(s["start"] for s in serial) >= max(p["end"] for p in par),
        str(rows),
    )
    shutil.rmtree(d, ignore_errors=True)


def gate_replay_crash(g, args, binary):
    print("== replay: crash replacement runs the remainder ==")
    d = _project({"test_crash.py": REPLAY_CRASH})
    log = d / "e2e.jsonl"
    t = "test_crash.py::test_"
    _journal(
        d,
        "crash.json",
        [[t + "before", t + "crash", t + "after_1", t + "after_2"], [t + "elsewhere"]],
    )
    r, rows = _replay(g, d, "crash.json", log)
    check("replay crash: plain failure (exit 1)", r.returncode == 1, r.stdout[-300:])
    check(
        "replay crash: gw0 ran each test once, in order",
        _ran(rows, "gw0") == ["before", "crash", "after_1", "after_2"],
        str(rows),
    )
    check("replay crash: 4 passed", "1 failed, 4 passed" in r.stdout, r.stdout[-300:])
    shutil.rmtree(d, ignore_errors=True)


def gate_replay_journal_validation(g, args, binary):
    print("== replay: journal validation ==")
    d = _project({"test_order.py": REPLAY_ORDER})
    j = {
        "schema": 1,
        "rstest_version": "gate",
        "run_uid": "bad",
        "created_epoch": 0,
        "workers": 3,
        "dist": "load",
        "shuffle_seed": None,
        "args": [],
        "collection_hash": None,
        "collection_size": 2,
        "assignment": [["test_order.py::test_other"], ["test_order.py::test_slow"]],
    }
    (d / "bad.json").write_text(json.dumps(j), encoding="utf-8")
    r = g.run("replay", "--journal", "bad.json", cwd=str(d))
    out = r.stdout + r.stderr
    check(
        "replay: mismatched workers rejected",
        r.returncode != 0 and "3 worker(s) but 2" in out,
        out[-300:],
    )
    shutil.rmtree(d, ignore_errors=True)


def gate_replay_side_effects(g, args, binary):
    print("== replay: what it leaves behind ==")
    d = _project({"test_order.py": REPLAY_ORDER})
    log = d / "e2e.jsonl"
    env = {"RSTEST_E2E_LOG": str(log)}
    replay_dir = d / ".rstest_cache" / "replay"
    g.run("-n", "2", "test_order.py", "-k", "not always_fails", cwd=str(d), env_extra=env)
    latest = replay_dir / "latest.json"
    before = latest.read_text(encoding="utf-8") if latest.exists() else None
    check("replay: parallel run recorded", before is not None)

    # A one-worker pool (-n 1 --reruns) has no schedule to replay and must not
    # overwrite the last parallel run's journal.
    g.run("-n", "1", "--reruns", "1", "test_order.py", cwd=str(d), env_extra=env)
    after = latest.read_text(encoding="utf-8") if latest.exists() else None
    check("replay: -n 1 --reruns leaves latest.json alone", after == before)

    # Repeated replays of a failing schedule are diagnostics, not history: no
    # failed counts piled into flakes.json, no durations rewritten.
    flakes = d / ".rstest_cache" / "flakes.json"
    durations = d / ".rstest_cache" / "durations.json"
    snap = lambda p: p.read_text(encoding="utf-8") if p.exists() else None  # noqa: E731
    f0, d0 = snap(flakes), snap(durations)
    t = "test_order.py::test_"
    _journal(d, "fail.json", [[t + "polluter", t + "victim"], [t + "other"]])
    codes = [_replay(g, d, "fail.json", log)[0].returncode for _ in range(3)]
    check("replay: failing journal fails each time", codes == [1, 1, 1], str(codes))
    check("replay: flakes.json untouched", snap(flakes) == f0, str(snap(flakes)))
    check("replay: durations.json untouched", snap(durations) == d0)

    # RSTEST_NO_REPLAY_JOURNAL=0 reads as "don't disable": still journals.
    shutil.rmtree(replay_dir, ignore_errors=True)
    g.run(
        "-n",
        "2",
        "test_order.py",
        "-k",
        "not always_fails",
        cwd=str(d),
        env_extra={**env, "RSTEST_NO_REPLAY_JOURNAL": "0"},
    )
    check("replay: RSTEST_NO_REPLAY_JOURNAL=0 keeps journaling", latest.exists())
    shutil.rmtree(d, ignore_errors=True)
