"""e2e gate sections: flaky."""

import json
import time

from _harness import CRASHFLAKY, FLAKY, MARKS, check, git, git_commit, git_init_commit


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
    # --auto-quarantine=0 here and below: earlier sections rescued this test
    # by rerun often enough that its history would auto-quarantine it.
    r = g.run(
        "test_flaky.py",
        "-n",
        "1",
        "--auto-quarantine=0",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(swm)},
    )
    check(
        "no-reruns at -n 1 stays byte-exact (flake fails)",
        r.returncode == 1
        and "1 failed" in r.stdout
        and "test session starts" in r.stdout.splitlines()[0],
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
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--auto-quarantine=0",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
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
            # Rerun history from earlier sections would auto-quarantine it.
            "--auto-quarantine=0",
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
            "--auto-quarantine=0",
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
        # 2 tests: the recovered one counts as flaky only, not also passed.
        r.returncode == 0 and "1 passed, 1 flaky" in r.stdout,
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
    r = g.run(".", "-n", "2", "--quarantine", "missing.txt", cwd=qdir)
    check(
        "quarantine: unreadable file is an error, not an empty list",
        r.returncode != 0 and "--quarantine: cannot read missing.txt" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
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
    # CI annotations must agree with the green exit: quarantined failures are
    # surfaced as warnings, never as ::error / logissue type=error.
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", "--output", "github", cwd=qdir)
    gh = r.stdout.splitlines()
    check(
        "quarantine: github annotates as ::warning, not ::error",
        r.returncode == 0
        and not any(ln.startswith("::error ") for ln in gh)
        and sum(ln.startswith("::warning ") and "quarantined" in ln for ln in gh) == 2,
        f"rc={r.returncode} " + r.stdout[-400:],
    )
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", "--output", "azure", cwd=qdir)
    az = [ln for ln in r.stdout.splitlines() if ln.startswith("##vso[task.logissue ")]
    check(
        "quarantine: azure logissue is type=warning, not type=error",
        r.returncode == 0
        and not any("type=error" in ln for ln in az)
        and sum("type=warning" in ln and "quarantined" in ln for ln in az) == 2,
        f"rc={r.returncode} " + r.stdout[-400:],
    )
    # Passthrough mode (-s) hands stdio to pytest and skips the quarantine
    # matcher entirely; the flag must warn it is being ignored, not silently
    # demote. Exit 1: the real bug still fails since nothing was demoted.
    r = g.run(".", "-n", "2", "-s", "--quarantine", "quarantine.txt", cwd=qdir)
    check(
        "quarantine: warns + no-op in passthrough mode",
        "--quarantine has no effect in passthrough mode" in r.stderr
        and "quarantined" not in r.stdout,
        f"rc={r.returncode} " + (r.stderr[-300:] or r.stdout[-300:]),
    )

    # --auto-quarantine: `test_flap` alternates pass/fail run to run (a counter
    # file), `test_broke` passes once then fails for good. After 6 runs flap has
    # flipped 3x and gets quarantined; broke flipped once and still fails.
    # Committed: a pass->fail flip only counts on one clean commit.
    g.write(
        "autoq/test_a.py",
        "import os, pathlib\n"
        "N = pathlib.Path(os.environ['AQ_COUNT'])\n\n"
        "def test_flap(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_broke(): assert int(N.read_text()) == 0\n",
    )
    adir = g.tmp / "autoq"
    git_init_commit(adir, "init")
    cnt = g.tmp / "autoq_count"
    for i in range(6):
        cnt.write_text(str(i))
        g.run(".", "-n", "2", cwd=adir, env_extra={"AQ_COUNT": str(cnt)})
    cnt.write_text("1")
    # On by default: no flag needed.
    aq_html = g.tmp / "autoq.html"
    r = g.run(".", "-n", "2", "--html", str(aq_html), cwd=adir, env_extra={"AQ_COUNT": str(cnt)})
    check(
        "auto-quarantine (default on): flapping test demoted, steady regression still fails",
        r.returncode == 1
        and "1 failed, 1 quarantined" in r.stdout
        and "QUARANTINED test_a.py::test_flap" in r.stdout
        and "--auto-quarantine: 1 test(s)" in r.stderr
        and "test_a.py::test_broke" not in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    doc = aq_html.read_text(encoding="utf-8")
    flap_row = next(
        (ln for ln in doc.splitlines() if 'data-nodeid="test_a.py::test_flap"' in ln), ""
    )
    check(
        "auto-quarantine: HTML report shows the auto badge, summary and strip",
        "1 test auto-quarantined" in doc
        and 'class="badge auto"' in flap_row
        and '<td class="strip">\u2713\u2717\u2713\u2717\u2713\u2717</td>' in flap_row,
        flap_row[:400],
    )
    # `rstest flaky` reads the same history: flap ranked first and marked,
    # broke shown as failing (broke and stays broken), not auto-quarantined.
    r = g.run("flaky", cwd=adir)
    lines = r.stdout.splitlines()

    def row(nodeid):
        return next((ln for ln in lines if nodeid in ln), "")

    check(
        "flaky: ranks flapper first, marks it, broke reads failing",
        r.returncode == 0
        and "2 flaky test(s) of 2 in the history" in r.stdout
        and lines.index(row("test_a.py::test_flap")) < lines.index(row("test_a.py::test_broke"))
        and "[auto-quarantined]" in row("test_a.py::test_flap")
        and " failing " in row("test_a.py::test_broke")
        and "[auto-quarantined]" not in row("test_a.py::test_broke"),
        f"rc={r.returncode} " + r.stdout[-500:],
    )
    r = g.run("flaky", "--json", "--top", "1", cwd=adir)
    fj = json.loads(r.stdout)
    check(
        "flaky --json: schema-stamped, --top cuts, total kept",
        fj["meta"]["kind"] == "flaky"
        and fj["flaky_tests"] == 2
        and [t["nodeid"] for t in fj["tests"]] == ["test_a.py::test_flap"]
        and fj["tests"][0]["auto_quarantined"] is True,
        r.stdout[-400:],
    )
    for flag, why in (
        ("--auto-quarantine=9", "threshold above the flips"),
        ("--auto-quarantine=0", "=0 turns it off"),
    ):
        r = g.run(".", "-n", "2", flag, cwd=adir, env_extra={"AQ_COUNT": str(cnt)})
        check(
            f"auto-quarantine: {why} quarantines nothing",
            r.returncode == 1 and "2 failed" in r.stdout and "quarantined" not in r.stdout,
            f"rc={r.returncode} " + r.stdout[-300:],
        )
    # The same flapping, but each run on a new commit: every fail follows a
    # code change, so nothing flips and nothing is quarantined.
    g.write(
        "autoq_edit/test_e.py",
        "import os, pathlib\n"
        "N = pathlib.Path(os.environ['AQ_COUNT'])\n\n"
        "def test_flap(): assert int(N.read_text()) % 2 == 0\n",
    )
    edir = g.tmp / "autoq_edit"
    git_init_commit(edir, "init")
    for i in range(1, 8):  # fail, pass, ... fail: 3 pass->fail steps
        cnt.write_text(str(i))
        (edir / "rev.txt").write_text(str(i))
        git(edir, "add", "rev.txt")
        git_commit(edir, f"edit {i}")
        r = g.run(".", "-n", "2", cwd=edir, env_extra={"AQ_COUNT": str(cnt)})
    check(
        "auto-quarantine: pass/fail across commits is not a flip",
        r.returncode == 1
        and "1 failed" in r.stdout
        and "quarantined" not in r.stdout
        and "--auto-quarantine:" not in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    cnt.write_text("1")
    # [tool.rstest]: the file path resolves against the pyproject's dir (run
    # from a subdir), and auto-quarantine = 0 turns the auto set off.
    g.write(
        "autoq/pyproject.toml",
        '[tool.rstest]\nquarantine = "q.txt"\nauto-quarantine = 0\n',
    )
    g.write("autoq/q.txt", "test_a.py::test_broke\n")
    g.write("autoq/sub/.keep", "")
    r = g.run("..", "-n", "2", cwd=adir / "sub", env_extra={"AQ_COUNT": str(cnt)})
    check(
        "auto-quarantine: [tool.rstest] quarantine + auto-quarantine = 0",
        r.returncode == 1
        and "1 failed, 1 quarantined" in r.stdout
        and "QUARANTINED test_a.py::test_broke" in r.stdout
        and "--auto-quarantine:" not in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    # Cap: a fresh project where both tests flap. A 10% cap of 2 tests rounds
    # up to 1, so 2 flippers are over it and nothing is quarantined.
    g.write(
        "autoq_cap/test_c.py",
        "import os, pathlib\n"
        "N = pathlib.Path(os.environ['AQ_COUNT'])\n\n"
        "def test_a(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_b(): assert int(N.read_text()) % 2 == 0\n",
    )
    cdir = g.tmp / "autoq_cap"
    git_init_commit(cdir, "init")
    for i in range(6):
        cnt.write_text(str(i))
        g.run(".", "-n", "2", "--auto-quarantine=0", cwd=cdir, env_extra={"AQ_COUNT": str(cnt)})
    cnt.write_text("1")
    r = g.run(
        ".", "-n", "2", "--auto-quarantine-max", "10", cwd=cdir, env_extra={"AQ_COUNT": str(cnt)}
    )
    check(
        "auto-quarantine: over the cap quarantines nothing and warns",
        r.returncode == 1
        and "2 failed" in r.stdout
        and "quarantined" not in r.stdout
        and "over the cap of 1 (10% of 2 tests in this run)" in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    # Over the cap under -x: the post-run demotion quarantines none of the
    # auto set, so the session's -x must count them too and stop, not run the
    # rest of the suite into a red result anyway. 2 flappers of 10 tests at
    # 10% (cap 1): the second auto failure stops the run.
    g.write(
        "autoq_x/test_x.py",
        "import os, pathlib\n"
        "N = pathlib.Path(os.environ['AQ_COUNT'])\n\n"
        "def test_a(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_b(): assert int(N.read_text()) % 2 == 0\n\n"
        + "".join(f"def test_ok{i}(): pass\n\n" for i in range(8)),
    )
    xdir = g.tmp / "autoq_x"
    git_init_commit(xdir, "init")
    for i in range(6):
        cnt.write_text(str(i))
        g.run(".", "-n", "0", "--auto-quarantine=0", cwd=xdir, env_extra={"AQ_COUNT": str(cnt)})
    cnt.write_text("1")
    r = g.run(
        ".",
        "-n",
        "0",
        "-x",
        "--auto-quarantine-max",
        "10",
        cwd=xdir,
        env_extra={"AQ_COUNT": str(cnt)},
    )
    check(
        "auto-quarantine: over the cap, -x stops on the auto failures",
        r.returncode == 1 and "2 failed" in r.stdout and "passed" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    # Under the cap, -x stopped by a real failure: the post-run demotion must
    # take the cap from the collected count like the in-run hold did, not from
    # the 3 tests that reported. 2 flappers of 40 tests at 5% (cap 2) are held,
    # test_c stops the run; a cap of 1 (5% of 3) would quarantine neither.
    g.write(
        "autoq_xcap/test_x.py",
        "import os, pathlib\n"
        "N = pathlib.Path(os.environ['AQ_COUNT'])\n\n"
        "def test_a(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_b(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_c(): assert False\n\n"
        + "".join(f"def test_ok{i}(): pass\n\n" for i in range(37)),
    )
    xcdir = g.tmp / "autoq_xcap"
    git_init_commit(xcdir, "init")
    for i in range(6):
        cnt.write_text(str(i))
        g.run(".", "-n", "0", "--auto-quarantine=0", cwd=xcdir, env_extra={"AQ_COUNT": str(cnt)})
    cnt.write_text("1")
    r = g.run(
        ".",
        "-n",
        "0",
        "-x",
        "--auto-quarantine-max",
        "5",
        cwd=xcdir,
        env_extra={"AQ_COUNT": str(cnt)},
    )
    check(
        "auto-quarantine: -x early stop keeps the collected-count cap",
        r.returncode == 1
        and "2 failures above are quarantined" in r.stdout
        and "over the cap" not in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    # Same under --collect lazy, where the test count is only final once every
    # file is collected. test_0.py sorts first, so one worker is stuck
    # importing it (AQ_SLOW) and the other collects test_flap.py and runs its
    # flappers meanwhile: their failures are held (the third waits as the
    # worker's lookahead). 2 held of 11 tests at 5% (cap 1): the last collect
    # settles the cap and must stop the run before test_0's tests start.
    g.write(
        "autoq_lazy/test_flap.py",
        "import os, pathlib\n"
        "N = pathlib.Path(os.environ['AQ_COUNT'])\n\n"
        "def test_a(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_b(): assert int(N.read_text()) % 2 == 0\n\n"
        "def test_c(): assert int(N.read_text()) % 2 == 0\n",
    )
    g.write(
        "autoq_lazy/test_0.py",
        "import os, time\n"
        "if os.environ.get('AQ_SLOW'): time.sleep(3)\n\n"
        + "".join(f"def test_ok{i}(): pass\n\n" for i in range(8)),
    )
    ldir = g.tmp / "autoq_lazy"
    git_init_commit(ldir, "init")
    for i in range(6):
        cnt.write_text(str(i))
        g.run(".", "-n", "0", "--auto-quarantine=0", cwd=ldir, env_extra={"AQ_COUNT": str(cnt)})
    cnt.write_text("1")
    r = g.run(
        ".",
        "-n",
        "2",
        "--collect",
        "lazy",
        "-x",
        "--auto-quarantine-max",
        "5",
        cwd=ldir,
        env_extra={"AQ_COUNT": str(cnt), "AQ_SLOW": "1"},
    )
    check(
        "auto-quarantine: lazy, the last collect settles the cap and stops -x",
        r.returncode == 1 and "2 failed" in r.stdout and "passed" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )
    # Lazy -x with reruns: a failure counts once its retries are spent, and
    # that final attempt stops the run.
    r = g.run(
        ".",
        "-n",
        "2",
        "--collect",
        "lazy",
        "-x",
        "--reruns",
        "1",
        "--auto-quarantine=0",
        cwd=ldir,
        env_extra={"AQ_COUNT": str(cnt)},
    )
    check(
        "lazy: -x stops on a failure whose reruns are spent",
        r.returncode == 1 and "stopping after 1 failures" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-300:],
    )


def gate_verify_vendor(g, args, binary):
    print("== verify-vendor ==")
    # Run-less integrity check: rehash the installed _vendor/ tree against the
    # packaged vendor.lock. Exits before any collection, so an empty cwd is fine.
    vdir = g.tmp / "vv"
    vdir.mkdir(parents=True, exist_ok=True)
    r = g.run("verify-vendor", cwd=vdir)
    check(
        "verify-vendor: intact tree verifies + exit 0",
        r.returncode == 0 and "verified against vendor.lock" in r.stdout,
        f"rc={r.returncode} " + (r.stdout[-300:] or r.stderr[-300:]),
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
