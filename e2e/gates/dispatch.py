"""e2e gate sections: dispatch."""

import re
import shutil
import xml.etree.ElementTree as ET

from _harness import (
    CRASHMANY,
    EACH_CRASH,
    FLAKY,
    HANG,
    LAZY_CONFTEST,
    LAZY_SESSION_A,
    LAZY_SESSION_B,
    LF,
    MAXFAIL,
    MAXFAIL_MANY,
    SCOPE_A,
    SCOPE_B,
    SCOPE_C,
    SERIAL,
    SERIAL_CRASH,
    STEAL,
    STEAL_SMALL,
    WINDOWS,
    check,
    clear_e2e_log,
    read_e2e_rows,
)


def gate_lazy_collection(g, args, binary):
    print("== lazy collection ==")
    # D5 single-point collection: same fixtures, same outcomes, no
    # initial collection pass in any worker.
    r = g.run("basic/test_basic.py", "-n", "2", "--collect", "lazy")
    check("lazy: parallel counts", "2 failed, 2 passed" in r.stdout, r.stdout[-200:])
    check("lazy: exit 1", r.returncode == 1)
    r = g.run("basic", "-n", "2", "--collect", "lazy", "-k", "passes")
    check(
        "lazy: -k filters per file",
        "2 passed" in r.stdout and "failed" not in r.stdout,
        r.stdout[-200:],
    )
    r = g.run("crash", "-n", "2", "--collect", "lazy")
    check("lazy: crash completes", "1 failed, 5 passed" in r.stdout, r.stdout[-200:])
    check("lazy: crash attributed", "crashed while running" in r.stdout, r.stdout[-300:])
    r = g.run(".", "-n", "2", "--collect", "lazy", cwd=g.tmp / "broken")
    check(
        "lazy: collect error aborts",
        r.returncode == 2,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    g.write("lazyfix/test_one.py", LAZY_SESSION_A)
    g.write("lazyfix/test_two.py", LAZY_SESSION_B)
    g.write("lazyfix/conftest.py", LAZY_CONFTEST)
    r = g.run("lazyfix", "-n", "2", "--collect", "lazy")
    check(
        "lazy: session fixture once per worker",
        "3 passed" in r.stdout and "failed" not in r.stdout,
        r.stdout[-300:],
    )
    r = g.run("empty", "--collect", "lazy", "-n", "2")
    check("lazy: no tests exit 5", r.returncode in (4, 5), f"rc={r.returncode}")
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker_lazy"
    if marker.exists():
        marker.unlink()
    g.write("flaky/test_flaky.py", FLAKY)
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--collect",
        "lazy",
        "--reruns",
        "2",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    check(
        "lazy: flaky passes with reruns",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )
    # lazy + --only-rerun: the failure text matches the regex, so the flaky test
    # is rerun-eligible and recovers (exercises the lazy rerun gate).
    marker.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--collect",
        "lazy",
        "--reruns",
        "2",
        "--only-rerun",
        "first attempt fails",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    check(
        "lazy: --only-rerun match reruns and recovers",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )
    # lazy + -x: the global maxfail trip halts dispatch and tells every worker
    # no_more_items (bounded overshoot), same coordination as the pool path.
    g.write("maxfail/test_maxfail.py", MAXFAIL)
    r = g.run("maxfail", "-n", "2", "--collect", "lazy", "-x", timeout=60)
    check(
        "lazy: -x trips global maxfail",
        r.returncode == 1 and "1 failed" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    log = g.tmp / "serial_lazy.jsonl"
    clear_e2e_log(log)
    g.write("serial/test_serial.py", SERIAL)
    r = g.run("serial", "-n", "3", "--collect", "lazy", env_extra={"RSTEST_E2E_LOG": str(log)})
    check("lazy: serial run green", "8 passed" in r.stdout, r.stdout[-200:])
    rows = read_e2e_rows(log)
    lser = [x for x in rows if x["name"].startswith("serial")]
    overlap = any(
        s["start"] < o["end"] and o["start"] < s["end"] for s in lser for o in rows if o is not s
    )
    check(
        "lazy: serial exclusive",
        not overlap and len({s["worker"] for s in lser}) == 1,
    )


def gate_serial_mark(g, args, binary):
    print("== serial mark ==")
    g.write("serial/test_serial.py", SERIAL)
    log = g.tmp / "serial.jsonl"
    clear_e2e_log(log)
    r = g.run("serial", "-n", "3", env_extra={"RSTEST_E2E_LOG": str(log)})
    check("serial run green", "8 passed" in r.stdout, r.stdout[-200:])
    rows = read_e2e_rows(log)
    serial = [x for x in rows if x["name"].startswith("serial")]
    par = [x for x in rows if x["name"].startswith("par")]
    overlap = any(
        s["start"] < o["end"] and o["start"] < s["end"] for s in serial for o in rows if o is not s
    )
    check("serial exclusive", not overlap and len({s["worker"] for s in serial}) == 1)
    check(
        "serial after parallel",
        min(s["start"] for s in serial) >= max(p["end"] for p in par),
    )


def gate_x_maxfail(g, args, binary):
    print("== -x / --maxfail ==")
    g.write("maxfail/test_maxfail.py", MAXFAIL)
    r = g.run("maxfail", "-n", "2", "-x", timeout=60)
    full = g.run("maxfail", "-n", "2", timeout=60)
    ran_x = int(r.stdout.split(" passed")[0].rsplit(" ", 1)[-1]) if " passed" in r.stdout else 0
    check("-x stops early", "1 failed" in r.stdout and ran_x < 8, r.stdout[-120:])
    check("full run unaffected", "8 passed" in full.stdout, full.stdout[-120:])


def gate_lf(g, args, binary):
    print("== --lf ==")
    lf = g.tmp / "lf"
    shutil.rmtree(lf / ".pytest_cache", ignore_errors=True)
    g.write("lf/test_lf.py", LF)
    g.run("test_lf.py", "-n", "2", cwd=lf)
    r = g.run("test_lf.py", "-n", "2", "--lf", cwd=lf)
    check(
        "--lf reruns only failures",
        "1 failed" in r.stdout and "passed" not in r.stdout,
        r.stdout[-200:],
    )


def gate_shard_k_n(g, args, binary):
    print("== --shard K/N ==")
    g.write("shardsuite/test_a.py", "".join(f"def test_a{i}(): assert True\n" for i in range(4)))
    g.write("shardsuite/test_b.py", "".join(f"def test_b{i}(): assert True\n" for i in range(4)))

    # Sibling shards MUST partition from an identical duration cache (in CI
    # each job restores the same snapshot). Run every shard in an isolated
    # cwd and wipe its cache first so all shards see the same cold cache --
    # otherwise shard 1 writes timings that shard 2 reads, and the partition
    # is no longer disjoint/covering.
    shard_cwd = g.tmp / "shardsuite"

    def shard_ids(k, n, *extra):
        shutil.rmtree(shard_cwd / ".rstest_cache", ignore_errors=True)
        tag = "_".join([str(k), str(n), *extra]).replace("/", "").replace("-", "")
        xp = g.tmp / f"shard_{tag}.xml"
        g.run(
            ".", "-n", "2", "--shard", f"{k}/{n}", "--junitxml", str(xp), *extra, cwd=str(shard_cwd)
        )
        root = ET.parse(xp).getroot()
        return {(tc.get("classname"), tc.get("name")) for tc in root.iter("testcase")}

    s1, s2 = shard_ids(1, 2), shard_ids(2, 2)
    check("shard: buckets disjoint", s1.isdisjoint(s2), f"overlap={s1 & s2}")
    check("shard: buckets cover the suite", len(s1 | s2) == 8, f"union={len(s1 | s2)}")
    check("shard: no empty bucket", bool(s1) and bool(s2), f"sizes={len(s1)},{len(s2)}")
    r = g.run("shardsuite", "-n", "2", "--shard", "5/4")
    check("shard: K>N rejected", r.returncode != 0 and "1..=4" in r.stderr, r.stderr[-160:])
    r = g.run("shardsuite", "-n", "2", "--shard", "1/2", "--shuffle")
    check(
        "shard: +shuffle rejected",
        r.returncode != 0 and "not supported with --shuffle" in r.stderr,
        r.stderr[-160:],
    )
    r = g.run("shardsuite", "-n", "2", "--shard", "1/1")
    check("shard: 1/1 runs whole suite", "8 passed" in r.stdout, r.stdout[-160:])

    # Lazy-collect shards at file granularity via shard_files.
    l1, l2 = shard_ids(1, 2, "--collect", "lazy"), shard_ids(2, 2, "--collect", "lazy")
    check("shard lazy: buckets disjoint", l1.isdisjoint(l2), f"overlap={l1 & l2}")
    check("shard lazy: buckets cover the suite", len(l1 | l2) == 8, f"union={len(l1 | l2)}")

    # Affinity mode (loadfile): a file's tests must never split across shards.
    def by_file(ids):
        files = {}
        for cls, name in ids:
            files.setdefault(cls, set()).add(name)
        return files

    f1 = by_file(shard_ids(1, 2, "--dist", "loadfile"))
    f2 = by_file(shard_ids(2, 2, "--dist", "loadfile"))
    split = list(set(f1) & set(f2))
    check("shard loadfile: no file split across shards", not split, f"split files={split}")
    all_files = set(f1) | set(f2)
    check("shard loadfile: buckets cover both files", len(all_files) == 2, f"files={all_files}")


def gate_dist_each(g, args, binary):
    print("== --dist each ==")
    r = g.run("basic/test_basic.py", "-n", "2", "--dist", "each")
    check("each: counts are per-worker", "4 failed, 4 passed" in r.stdout, r.stdout[-200:])
    check("each: exit 1", r.returncode == 1)
    check(
        "each: outcomes keyed per worker",
        "[gw0]" in r.stdout and "[gw1]" in r.stdout,
        r.stdout[-400:],
    )
    r = g.run("basic/test_basic.py", "-n", "2", "--dist", "each", "--reruns", "2")
    check(
        "each: --reruns rejected",
        r.returncode != 0 and "not supported" in r.stderr,
        r.stderr[-200:],
    )


def gate_dist_validation(g, args, binary):
    print("== --dist validation ==")
    # An invalid --dist value must be rejected the same way on every path;
    # the small-suite/lazy path used to accept garbage silently (exit 0).
    r = g.run("basic/test_basic.py", "--dist", "bogus")
    check(
        "dist: bogus rejected on small/lazy path",
        r.returncode != 0 and "unknown --dist mode" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run("basic/test_basic.py", "-n", "2", "--dist", "bogus")
    check(
        "dist: bogus rejected on pool path",
        r.returncode != 0 and "unknown --dist mode" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )


def gate_loadscope_loadgroup(g, args, binary):
    print("== loadscope / loadgroup ==")
    g.write("scopes/test_sc_a.py", SCOPE_A)
    g.write("scopes/test_sc_b.py", SCOPE_B)
    g.write("scopes/test_sc_c.py", SCOPE_C)
    slog = g.tmp / "scope_log"
    for mode, _label in (("loadscope", "class"), ("loadgroup", "group")):
        clear_e2e_log(slog)
        r = g.run("-n", "3", "--dist", mode, cwd=g.tmp / "scopes", env_extra={"SLOG": str(slog)})
        rows = read_e2e_rows(slog)
        import collections

        by = collections.defaultdict(set)
        for x in rows:
            by[x["t"].split(".")[0]].add(x["w"])
        if mode == "loadscope":
            check(
                "loadscope: classes cohesive",
                len(by["alpha"]) == 1 and len(by["beta"]) == 1 and "12 passed" in r.stdout,
                str(dict(by)),
            )
        else:
            check(
                "loadgroup: cross-file group cohesive",
                len(by["grp"]) == 1 and "12 passed" in r.stdout,
                str(dict(by)),
            )


def gate_auto_worker_capping(g, args, binary):
    print("== auto worker capping ==")
    for i in range(6):
        g.write(f"kovstyle/mod{i}_test.py", "def test_a(): assert True\n")
    r = g.run(cwd=g.tmp / "kovstyle")
    check(
        "auto scales on *_test.py suites",
        "workers (parallel" in r.stdout.splitlines()[0],
        r.stdout[:100],
    )
    g.write("tiny/test_one.py", "def test_only(): assert True\n")
    r = g.run(cwd=g.tmp / "tiny")
    check(
        "auto caps tiny suite to single worker",
        "single worker" in r.stdout.splitlines()[0],
        r.stdout[:100],
    )


def gate_shuffle(g, args, binary):
    print("== shuffle ==")
    for i in range(6):
        g.write(f"shuf/test_s{i}.py", f"def test_s{i}(): assert True\n")
    r = g.run("shuf", "-n", "2", "--shuffle=42")
    check(
        "shuffle: explicit seed echoed, run green",
        r.returncode == 0
        and "shuffle seed 42" in r.stderr
        and "--shuffle=42" in r.stderr
        and "6 passed" in r.stdout,
        r.stderr[-200:] + r.stdout[-100:],
    )
    r = g.run("shuf", "-n", "2", "--shuffle")
    check(
        "shuffle: random seed printed with reproduce hint",
        r.returncode == 0 and "reproduce with --shuffle=" in r.stderr,
        r.stderr[-200:],
    )
    r = g.run("shuf", "-n", "0", "--shuffle")
    check(
        "shuffle: refused in single-worker mode",
        r.returncode != 0 and "parallel pool" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )


def gate_duration_regression_gate(g, args, binary):
    print("== duration regression gate ==")
    g.write(
        "dreg/test_d.py",
        "import os, time\n\n"
        "def test_variable():\n"
        "    time.sleep(float(os.environ.get('DREG_SLEEP', '0.1')))\n\n"
        "def test_stable():\n"
        "    time.sleep(0.05)\n",
    )
    ddir = g.tmp / "dreg"
    r = g.run(
        ".", "-n", "2", "--durations-regress", "2.0", cwd=ddir, env_extra={"DREG_SLEEP": "0.1"}
    )
    check(
        "durations-regress: cold baseline skips, run green",
        r.returncode == 0 and "no duration baseline yet" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run(
        ".", "-n", "2", "--durations-regress", "2.0", cwd=ddir, env_extra={"DREG_SLEEP": "0.1"}
    )
    check(
        "durations-regress: warm baseline, no regressions",
        r.returncode == 0 and "no regressions" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run(
        ".", "-n", "2", "--durations-regress", "2.0", cwd=ddir, env_extra={"DREG_SLEEP": "1.2"}
    )
    check(
        "durations-regress: regression flagged, exit 1, stable test quiet",
        r.returncode == 1
        and "duration regressions" in r.stdout
        and "test_variable" in r.stdout
        and "test_stable" not in r.stdout.split("duration regressions")[1]
        and "1 duration regression" in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-150:],
    )


def gate_native_timeout(g, args, binary):
    print("== native per-test timeout (--timeout) ==")
    tp = g.tmp / "toproj"
    g.write(
        "toproj/test_to.py",
        "import time, pytest\n"
        "def test_fast(): assert True\n"
        "def test_slow():\n"
        "    time.sleep(5)\n"
        "    assert True\n"
        "@pytest.mark.timeout(0.3)\n"
        "def test_marked():\n"
        "    time.sleep(5)\n"
        "    assert True\n",
    )
    r = g.run("test_to.py", "-n", "2", "--timeout", "1", cwd=tp, env_extra={"PYTHONPATH": str(tp)})
    # In-process interrupt of a blocked test needs SIGALRM firing INSIDE the
    # stuck syscall — Unix-only. Windows has no equivalent, so native per-test
    # --timeout is a no-op there (the --worker-timeout watchdog is the backstop,
    # see gate_worker_timeout_watchdog). Assert the in-process behavior only
    # where it can hold.
    if not WINDOWS:
        check(
            "timeout: slow test fails in-process with a traceback at the stuck line",
            r.returncode == 1
            and "2 failed, 1 passed" in r.stdout
            and "test_to.py::test_slow" in r.stdout
            and "exceeded --timeout (1s)" in r.stdout
            and "time.sleep(5)" in r.stdout,  # traceback points at the blocked line
            r.stdout[-500:],
        )
        check(
            "timeout: @pytest.mark.timeout overrides the global value",
            "exceeded --timeout (0.3s)" in r.stdout,
            r.stdout[-500:],
        )
    else:
        # Windows: --timeout can't fire in-process, so the orchestrator warns
        # the user once and points at the --worker-timeout backstop.
        check(
            "timeout: Windows warns --timeout is not enforced in-process",
            "--timeout can't interrupt a blocked test in-process on Windows" in r.stderr
            and "--worker-timeout" in r.stderr,
            r.stderr[-500:],
        )
    # A suite that finishes under the deadline is unaffected.
    g.write("toproj/test_ok.py", "def test_a(): assert True\ndef test_b(): assert True\n")
    r = g.run("test_ok.py", "-n", "2", "--timeout", "5", cwd=tp, env_extra={"PYTHONPATH": str(tp)})
    check(
        "timeout: fast suite passes, no timeout",
        r.returncode == 0 and "2 passed" in r.stdout and "timeout" not in r.stdout.lower(),
        r.stdout[-200:],
    )


def gate_worker_timeout_watchdog(g, args, binary):
    print("== worker-timeout watchdog ==")
    g.write("hang/test_hang.py", HANG)
    r = g.run("test_hang.py", "-n", "2", "--worker-timeout", "3", cwd=g.tmp / "hang", timeout=60)
    check(
        "hung test killed and attributed",
        r.returncode == 1 and "exceeded --worker-timeout" in r.stdout and "2 passed" in r.stdout,
        r.stdout[-300:],
    )


def _count(stdout, word):
    """Parse the 'N word' figure from the summary line (0 if absent). Uses the
    LAST match so a traceback mentioning the word can't shadow the summary."""
    matches = re.findall(rf"(\d+) {word}\b", stdout)
    return int(matches[-1]) if matches else 0


def gate_maxfail_bound(g, args, binary):
    # Precision beyond gate_x_maxfail's "< 8": at -n 1 there is no second
    # worker to overshoot, so --maxfail=K reports EXACTLY K failures and leaves
    # the rest unrun; at -n 2 the overshoot stays bounded (never the full suite).
    print("== maxfail bound ==")
    g.write("mfmany/test_mf.py", MAXFAIL_MANY)
    r = g.run("mfmany", "-n", "1", "--maxfail", "3", timeout=60)
    check(
        "maxfail: -n1 stops at exactly K",
        _count(r.stdout, "failed") == 3 and r.returncode == 1,
        r.stdout[-200:],
    )
    r = g.run("mfmany", "-n", "2", "--maxfail", "2", timeout=60)
    failed = _count(r.stdout, "failed")
    # >= maxfail (the trip), but bounded: two workers can overshoot by at most
    # their in-flight items, never the whole 6-fail suite.
    check(
        "maxfail: -n2 overshoot bounded",
        r.returncode == 1 and 2 <= failed < 6,
        f"failed={failed} " + r.stdout[-200:],
    )


def gate_lazy_work_stealing(g, args, binary):
    # One 30-item file, more workers than files. --dist load (steal ON) forces
    # idle workers to steal from the collector's own_queue -> work spreads across
    # >=2 workers. --dist loadfile (steal OFF) pins the whole file to its
    # collecting worker -> exactly one worker runs it. The contrast proves both
    # the steal branch AND its affinity guard.
    print("== lazy work stealing ==")
    g.write("steal/test_big.py", STEAL)
    g.write("steal/test_small.py", STEAL_SMALL)
    log = g.tmp / "steal_on.jsonl"
    clear_e2e_log(log)
    r = g.run(
        "steal",
        "-n",
        "2",
        "--collect",
        "lazy",
        "--dist",
        "load",
        env_extra={"RSTEST_E2E_LOG": str(log)},
        timeout=60,
    )
    workers_on = {row["worker"] for row in read_e2e_rows(log)}
    check(
        "steal ON: all 41 run and big file spreads across workers",
        "41 passed" in r.stdout and len(workers_on) >= 2,
        f"workers={workers_on} " + r.stdout[-150:],
    )
    log = g.tmp / "steal_off.jsonl"
    clear_e2e_log(log)
    r = g.run(
        "steal",
        "-n",
        "2",
        "--collect",
        "lazy",
        "--dist",
        "loadfile",
        env_extra={"RSTEST_E2E_LOG": str(log)},
        timeout=60,
    )
    workers_off = {row["worker"] for row in read_e2e_rows(log)}
    check(
        "steal OFF: affinity pins the big file to one worker",
        "41 passed" in r.stdout and len(workers_off) == 1,
        f"workers={workers_off} " + r.stdout[-150:],
    )


def gate_dist_each_crash_remnant(g, args, binary):
    # --dist each + a crash: gw0 dies once on test_crashes_once; its replacement
    # must run only the REMAINING item (test_other) from each_remnant, while gw1
    # runs the full suite. Net: the crashed item fails once [gw0], the other
    # three cells pass. Guards the each-mode remnant reseed path.
    print("== dist each crash remnant ==")
    g.write("eachcrash/test_ec.py", EACH_CRASH)
    marker = g.tmp / "each_crash_marker"
    marker.unlink(missing_ok=True)
    r = g.run(
        "eachcrash",
        "-n",
        "2",
        "--dist",
        "each",
        env_extra={"CRASH_MARKER": str(marker)},
        timeout=60,
    )
    # 4 cells (2 workers x 2 tests): the crashed one fails once, the other three
    # pass -> proves gw1 ran the full suite AND gw0's replacement ran the remnant.
    check(
        "each+crash: 1 failed, 3 passed",
        _count(r.stdout, "failed") == 1 and _count(r.stdout, "passed") == 3,
        r.stdout[-250:],
    )
    # Only failures print their nodeid in default output; the crash is keyed to
    # the worker that died and attributed. (Passing cells don't echo [gwN].)
    check(
        "each+crash: crash keyed to its worker and attributed",
        "[gw0]" in r.stdout and "crashed while running" in r.stdout,
        r.stdout[-400:],
    )
    check("each+crash: exit 1", r.returncode == 1)


def gate_serial_after_crash(g, args, binary):
    # A non-designate worker (gw1) crashes during the parallel phase. The
    # designate (gw0) survives, so the serial phase must still activate once all
    # parallel work resolves and run the serial tests exclusively, after parallel.
    print("== serial after crash ==")
    g.write("serialcrash/test_sc.py", SERIAL_CRASH)
    log = g.tmp / "serial_crash.jsonl"
    clear_e2e_log(log)
    marker = g.tmp / "serial_crash_marker"
    marker.unlink(missing_ok=True)
    r = g.run(
        "serialcrash",
        "-n",
        "2",
        env_extra={"RSTEST_E2E_LOG": str(log), "CRASH_MARKER": str(marker)},
        timeout=60,
    )
    check(
        "serial-after-crash: crash attributed, others green",
        _count(r.stdout, "failed") == 1 and "crashed while running" in r.stdout,
        r.stdout[-250:],
    )
    rows = read_e2e_rows(log)
    serial = [x for x in rows if x["name"].startswith("serial")]
    par = [x for x in rows if x["name"].startswith("par")]
    overlap = any(
        s["start"] < o["end"] and o["start"] < s["end"] for s in serial for o in rows if o is not s
    )
    check(
        "serial-after-crash: both serial ran, exclusive, on one worker",
        len(serial) == 2 and not overlap and len({s["worker"] for s in serial}) == 1,
        f"serial={serial}",
    )
    check(
        "serial-after-crash: serial runs after all parallel",
        bool(serial)
        and bool(par)
        and min(s["start"] for s in serial) >= max(p["end"] for p in par),
    )


def gate_crash_restart_exhaustion(g, args, binary):
    # Ten crashers, restart budget is n.max(4)=4. Once the budget is spent a
    # further crash is non-restartable: the worker is recorded as an internal
    # error (pytest exit 3), not respawned. Guards the else (give-up) crash arm.
    print("== crash restart exhaustion ==")
    g.write("crashmany/test_cm.py", CRASHMANY)
    r = g.run("crashmany", "-n", "2", timeout=90)
    check(
        "restart budget exhausts -> internal error exit 3",
        r.returncode == 3 and "terminated unexpectedly" in (r.stdout + r.stderr),
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-300:],
    )
