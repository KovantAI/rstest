"""e2e gate sections: flaky."""

import json
import time

from _harness import CRASHFLAKY, FLAKY, MARKS, check


def gate_flaky_reruns(g, args, binary):
    print("== flaky reruns ==")
    g.write("flaky/test_flaky.py", FLAKY)
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker"
    if marker.exists():
        marker.unlink()
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    check("flaky passes with reruns", r.returncode == 0 and "1 flaky" in r.stdout, r.stdout[-200:])
    check("flaky section listed", "passed after rerun" in r.stdout)
    marker.unlink()

    # buildkite_flaky_annotate: with BUILDKITE set and a flaky-passed test, rstest
    # builds a flaky annotation and hands it to `buildkite-agent annotate`; absent
    # that binary (CI/gate runners), it best-effort-skips with a stderr notice.
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--output",
        "buildkite",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker), "BUILDKITE": "1"},
    )
    check(
        "buildkite: flaky annotation attempted (best-effort skip without agent)",
        r.returncode == 0 and "1 flaky" in r.stdout and "Buildkite flaky annotation" in r.stderr,
        f"rc={r.returncode} " + r.stdout[-200:] + " || " + r.stderr[-200:],
    )
    marker.unlink(missing_ok=True)

    # Single-worker reruns: --reruns at -n 1 / -n 0 must fire (a degenerate
    # one-worker pool drives the rerun loop) instead of being silently inert.
    swm = g.tmp / "sw_reruns_marker"
    swm.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py", "-n", "1", "--reruns", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(swm)}
    )
    check(
        "single-worker reruns fire at -n 1",
        r.returncode == 0
        and "1 flaky" in r.stdout
        and "single worker (rerun pool" in r.stdout.splitlines()[0]
        # the byte-exact -> pool switch is announced on stderr, not just the banner
        and "not byte-exact" in r.stderr,
        f"rc={r.returncode} " + r.stdout.splitlines()[0] + " || " + r.stderr[-200:],
    )
    swm.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py", "-n", "0", "--reruns", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(swm)}
    )
    check(
        "single-worker reruns fire at -n 0",
        r.returncode == 0 and "1 flaky" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    # No --reruns at -n 1 stays byte-exact single session: the flake fails.
    swm.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "1", cwd=fdir, env_extra={"FLAKY_MARKER": str(swm)})
    check(
        "no-reruns at -n 1 stays byte-exact (flake fails)",
        r.returncode == 1
        and "1 failed" in r.stdout
        and "pytest-exact mode" in r.stdout.splitlines()[0],
        f"rc={r.returncode} " + r.stdout.splitlines()[0] + " || " + r.stdout[-200:],
    )
    # Passthrough (-s) can't be pooled: reruns stay inert, warned.
    swm.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py",
        "-n",
        "1",
        "--reruns",
        "2",
        "-s",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(swm)},
    )
    check(
        "reruns inert under -s, warned",
        r.returncode == 1 and "ignored under -s" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    marker.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(marker)})
    check(
        "flaky fails without reruns", r.returncode == 1 and "1 failed" in r.stdout, r.stdout[-200:]
    )


def gate_flaky_aware_reruns_reruns_only_known_fla(g, args, binary):
    print("== flaky-aware reruns (--reruns-only-known-flaky) ==")
    g.write("flaky/test_flaky.py", FLAKY)  # self-contained (also written by gate_flaky_reruns)
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker"
    # Gate reruns on prior flaky history so a deterministic mass-failure
    # doesn't burn the budget. Fixture: test_flaky_once fails its first
    # attempt then passes; nodeid is `test_flaky.py::test_flaky_once`.
    fnode = "test_flaky.py::test_flaky_once"
    fcache = g.tmp / "flaky" / ".rstest_cache" / "flakes.json"
    fcache.parent.mkdir(parents=True, exist_ok=True)

    def faware(hist, marker_name, *extra):
        # hist: dict written to flakes.json (or None to remove it).
        m = g.tmp / marker_name
        m.unlink(missing_ok=True)
        if hist is None:
            fcache.unlink(missing_ok=True)
        else:
            fcache.write_text(json.dumps(hist), encoding="utf-8")
        r = g.run(
            "test_flaky.py",
            "-n",
            "2",
            "--reruns",
            "2",
            "--reruns-only-known-flaky",
            *extra,
            cwd=fdir,
            env_extra={"FLAKY_MARKER": str(m)},
        )
        m.unlink(missing_ok=True)
        return r

    # In history with flaky>0 -> rerun-eligible -> recovers. Use a current
    # epoch so retention aging (load() drops entries past the window) keeps it.
    now_epoch = int(time.time())
    r = faware({fnode: {"flaky": 2, "failed": 0, "last_epoch": now_epoch}}, "fa_known")
    check(
        "flaky-aware: known-flaky test is reran and recovers",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )
    # No history -> not known-flaky -> not reran -> fails.
    r = faware(None, "fa_unknown")
    check(
        "flaky-aware: unknown test not reran, fails",
        r.returncode == 1 and "1 failed" in r.stdout and "passed after rerun" not in r.stdout,
        r.stdout[-200:],
    )
    # Hard-failure-only history (flaky==0) -> still not known-flaky: a
    # deterministic mass-failure recorded as `failed` never burns the budget.
    r = faware({fnode: {"flaky": 0, "failed": 9, "last_epoch": now_epoch}}, "fa_failedonly")
    check(
        "flaky-aware: failed-only history does not count as known-flaky",
        r.returncode == 1 and "1 failed" in r.stdout,
        r.stdout[-200:],
    )
    # Baseline sanity: same unknown test WITHOUT the flag reruns and recovers.
    m = g.tmp / "fa_baseline"
    m.unlink(missing_ok=True)
    fcache.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py", "-n", "2", "--reruns", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(m)}
    )
    m.unlink(missing_ok=True)
    check(
        "flaky-aware: without the flag, unknown flake still recovers",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )

    # An explicit @pytest.mark.flaky declaration bypasses the gate even with
    # no history: the author already declared it flaky.
    g.write(
        "faware_mark/test_m.py",
        "import os, pathlib, pytest\n"
        "@pytest.mark.flaky(reruns=2)\n"
        "def test_marked():\n"
        "    m = pathlib.Path(os.environ['MK'])\n"
        "    if not m.exists():\n"
        "        m.write_text('x'); assert False\n"
        "    assert True\n",
    )
    mdir = g.tmp / "faware_mark"
    mk = g.tmp / "fa_mark"
    mk.unlink(missing_ok=True)
    # --reruns 2 so the gate is actually active (known_flaky loads); with no
    # history an unmarked test would be blocked, so recovery here proves the
    # @mark.flaky declaration bypasses the gate.
    r = g.run(
        "test_m.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--reruns-only-known-flaky",
        cwd=mdir,
        env_extra={"MK": str(mk)},
    )
    mk.unlink(missing_ok=True)
    check(
        "flaky-aware: @mark.flaky bypasses the gate (no history)",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )

    # Cold-start loop (finding #4 / known defect): the docs once claimed a
    # brand-new flake "fails that run, is recorded, and is rescued on
    # subsequent runs". It is NOT. The gate suppresses the rerun that would
    # record `flaky > 0`, so a gated run only ever records the failure as
    # `failed` (flaky == 0) -> the test stays unknown -> the NEXT gated run
    # gates it again. The corrected docs describe the real two-mode workflow.
    #
    # This test PINS that current behavior (run 2 still fails), so gate.py
    # stays green today. It is the tripwire for the fix: when a
    # learn-without-rerun mechanism lands and cold-start self-heals, run 2
    # will start passing and the run-2 assertion below will flip to FAIL —
    # at which point update it to assert recovery (rc == 0, "1 flaky").
    cs_marker = "fa_coldstart"

    def coldstart_run():
        m = g.tmp / cs_marker
        m.unlink(missing_ok=True)  # fresh: fixture fails its first attempt
        r = g.run(
            "test_flaky.py",
            "-n",
            "2",
            "--reruns",
            "2",
            "--reruns-only-known-flaky",
            cwd=fdir,
            env_extra={"FLAKY_MARKER": str(m)},
        )
        m.unlink(missing_ok=True)
        return r

    fcache.unlink(missing_ok=True)  # no seeded history: truly cold
    r1 = coldstart_run()
    # Run 1: unknown -> gated -> fails, and the failure IS recorded so we know
    # the miss is the gate, not a missing write.
    hist_after = json.loads(fcache.read_text()) if fcache.exists() else {}
    rec = hist_after.get(fnode, {})
    check(
        "flaky-aware cold-start: run 1 gated-fails and records the failure",
        r1.returncode == 1 and rec.get("failed", 0) > 0 and rec.get("flaky", 0) == 0,
        f"rc={r1.returncode} rec={rec} {r1.stdout[-160:]}",
    )
    # Run 2: same flag, history now carries the run-1 failure (flaky == 0).
    # A self-healing feature WOULD rescue it here; today it does not. Assert
    # the current (defective) behavior so CI stays green. When cold-start is
    # fixed this flips red -> update to assert rc == 0 and "1 flaky".
    r2 = coldstart_run()
    check(
        "flaky-aware cold-start: run 2 still gated-fails (pins finding #4; flip when fixed)",
        r2.returncode == 1 and "1 failed" in r2.stdout and "passed after rerun" not in r2.stdout,
        r2.stdout[-200:],
    )
    fcache.unlink(missing_ok=True)

    g.write("crashflaky/test_cf.py", CRASHFLAKY)
    cmarker = g.tmp / "cf_marker"
    if cmarker.exists():
        cmarker.unlink()
    r = g.run(
        "test_cf.py",
        "-n",
        "2",
        "--reruns",
        "1",
        cwd=g.tmp / "crashflaky",
        env_extra={"FLAKY_MARKER": str(cmarker)},
    )
    check(
        "crashed test retried within budget",
        r.returncode == 0 and "1 flaky" in r.stdout and "2 passed" in r.stdout,
        r.stdout[-200:],
    )
    marker.unlink(missing_ok=True)
    fx = g.tmp / "flaky_junit.xml"
    g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--junitxml",
        str(fx),
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    check(
        "flaky flagged in junit property",
        'property name="flaky"' in fx.read_text(encoding="utf-8"),
        fx.read_text(encoding="utf-8")[-300:],
    )
    # --output github: a flaky-passed test surfaces as a ::warning
    # annotation (the run is green, the flake is visible on the PR).
    marker.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--output",
        "github",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    warns = [ln for ln in r.stdout.splitlines() if ln.startswith("::warning ")]
    check(
        "github: flaky-passed emits ::warning",
        r.returncode == 0
        and len(warns) == 1
        and "flaky" in warns[0]
        and "rerun" in warns[0]
        and "test_flaky.py" in warns[0]
        and not any(ln.startswith("::error ") for ln in r.stdout.splitlines()),
        r.stdout[-300:],
    )


def gate_quarantine(g, args, binary):
    print("== quarantine ==")
    g.write(
        "quar/test_q.py",
        "def test_ok(): assert True\n\n"
        "def test_known_flake(): assert False, 'known flake'\n\n"
        "def test_real_bug(): assert 1 == 2\n",
    )
    qdir = g.tmp / "quar"
    g.write("quar/quarantine.txt", "# known flakes\ntest_q.py::test_known_flake\n")
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", cwd=qdir)
    check(
        "quarantine: listed failure demoted, unlisted still fails",
        r.returncode == 1
        and "1 failed, 1 passed, 1 quarantined" in r.stdout
        and "QUARANTINED test_q.py::test_known_flake" in r.stdout
        and "FAILED" in r.stdout
        and "QUARANTINED test_q.py::test_real_bug" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-400:],
    )
    g.write("quar/quarantine.txt", "test_q.py::*\n")
    qx = g.tmp / "quar_junit.xml"
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", "--junitxml", str(qx), cwd=qdir)
    jx = qx.read_text(encoding="utf-8")
    check(
        "quarantine: glob demotes all -> exit 0, junit green + flagged",
        r.returncode == 0
        and "2 quarantined" in r.stdout
        and 'failures="0"' in jx
        and jx.count('property name="quarantined"') == 2,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    flog = json.loads((qdir / ".rstest_cache" / "flakes.json").read_text(encoding="utf-8"))
    check(
        "flake history: failures recorded across runs",
        flog.get("test_q.py::test_real_bug", {}).get("failed", 0) >= 2,
        str(flog)[:300],
    )
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", cwd=qdir)
    check(
        "quarantine: history annotation in section",
        "failed 2x before" in r.stdout or "failed 3x before" in r.stdout,
        r.stdout[-400:],
    )


def gate_flaky_marks_only_rerun(g, args, binary):
    print("== flaky marks / only-rerun ==")
    g.write("marks/test_marks.py", MARKS)
    mk = g.tmp / "marks_marker"
    cnt = g.tmp / "marks_count"
    for f in (mk, cnt):
        if f.exists():
            f.unlink()
    r = g.run(
        "test_marks.py", "-n", "2", cwd=g.tmp / "marks", env_extra={"MK": str(mk), "CNT": str(cnt)}
    )
    check(
        "flaky mark reruns without --reruns",
        "1 flaky" in r.stdout and "1 failed" in r.stdout,
        r.stdout[-200:],
    )
    check(
        "unmarked test not rerun by mark",
        cnt.read_text() == "1",
        cnt.read_text(),
    )
    mk.unlink()
    cnt.unlink()
    r = g.run(
        "test_marks.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--only-rerun",
        "transient",
        cwd=g.tmp / "marks",
        env_extra={"MK": str(mk), "CNT": str(cnt)},
    )
    check(
        "only-rerun gates non-matching failures",
        "1 flaky" in r.stdout and cnt.read_text() == "1",
        f"count={cnt.read_text()} " + r.stdout[-160:],
    )
