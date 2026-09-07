"""e2e gate sections: incremental."""

import json
import subprocess

from _harness import check, git, git_commit, git_init_commit


def _since_green_baseline(proj):
    """The recorded last-green commit sha, or None if no baseline file yet."""
    p = proj / ".rstest_cache" / "last_green.json"
    if not p.exists():
        return None
    return json.loads(p.read_text())["sha"]


def _head_sha(proj):
    return subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=proj, capture_output=True, text=True, check=True
    ).stdout.strip()


def gate_since_green_incremental(g, args, binary):
    print("== since-green incremental ==")
    sp = g.tmp / "incrproj"
    g.write("incrproj/test_alpha.py", "def test_a():\n    assert 1 == 1\n")
    g.write("incrproj/test_beta.py", "def test_b():\n    assert 2 == 2\n")
    g.write("incrproj/pyproject.toml", "[tool.pytest.ini_options]\n")
    git_init_commit(sp, "init")
    env = {"PYTHONPATH": str(sp)}

    # Run 1: no baseline -> establish it by running everything, then record HEAD.
    r = g.run("--since-green", "-n", "2", cwd=sp, env_extra=env)
    check(
        "since-green: first run has no baseline -> full run",
        "no prior green run recorded" in r.stderr and "2 passed" in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )
    check(
        "since-green: green run records baseline == HEAD",
        _since_green_baseline(sp) == _head_sha(sp),
        f"baseline={_since_green_baseline(sp)} head={_head_sha(sp)}",
    )

    # Run 2: nothing changed since the green baseline -> select nothing.
    r = g.run("--since-green", "-n", "2", cwd=sp, env_extra=env)
    check(
        "since-green: no changes since green -> nothing affected",
        r.returncode == 0
        and "selecting changes since last green run" in r.stderr
        and "no tests affected" in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Run 3: edit + commit one test -> only it is selected; baseline advances.
    g.write("incrproj/test_beta.py", "def test_b():\n    assert 2 + 0 == 2  # touched\n")
    git(sp, "add", "-A")
    git_commit(sp, "touch-beta")
    r = g.run("--since-green", "-n", "2", "-v", cwd=sp, env_extra=env)
    check(
        "since-green: only the changed test is selected",
        "1 affected test target(s)" in r.stderr
        and "test_b" in r.stdout
        and "test_a" not in r.stdout,
        r.stderr[-200:] + r.stdout[-300:],
    )
    check(
        "since-green: passing run advances baseline to new HEAD",
        _since_green_baseline(sp) == _head_sha(sp),
        f"baseline={_since_green_baseline(sp)} head={_head_sha(sp)}",
    )

    # Run 4: break the test -> failing run must NOT advance the baseline.
    baseline_before = _since_green_baseline(sp)
    g.write("incrproj/test_beta.py", "def test_b():\n    assert 2 == 3  # boom\n")
    git(sp, "add", "-A")
    git_commit(sp, "break-beta")
    r = g.run("--since-green", "-n", "2", cwd=sp, env_extra=env)
    check(
        "since-green: failing run holds the baseline",
        r.returncode == 1 and _since_green_baseline(sp) == baseline_before,
        f"rc={r.returncode} baseline={_since_green_baseline(sp)} before={baseline_before}",
    )

    # Run 5: fix the test -> green again advances the baseline to HEAD.
    g.write("incrproj/test_beta.py", "def test_b():\n    assert 2 == 2  # fixed\n")
    git(sp, "add", "-A")
    git_commit(sp, "fix-beta")
    r = g.run("--since-green", "-n", "2", cwd=sp, env_extra=env)
    check(
        "since-green: recovery run advances baseline again",
        r.returncode == 0 and _since_green_baseline(sp) == _head_sha(sp),
        f"rc={r.returncode} baseline={_since_green_baseline(sp)} head={_head_sha(sp)}",
    )

    # Run 6: an environment change git can't see (a new lockfile), with NO
    # source change, must bust the baseline -> full run, not a false
    # "nothing affected". This is the fingerprint guard against sticky
    # false-greens after a dependency upgrade.
    g.write("incrproj/uv.lock", "version = 1\n")
    r = g.run("--since-green", "-n", "2", cwd=sp, env_extra=env)
    check(
        "since-green: dependency change busts the baseline -> full run",
        r.returncode == 0 and "no prior green run recorded" in r.stderr and "2 passed" in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Run 7: --changed-strict must NOT hijack --since-green's diff base. Commit a
    # source change so the working tree is CLEAN; strict would otherwise imply a
    # "HEAD" (working-tree) base and see nothing. --since-green owns the base, so
    # the last-green baseline is still used -> the changed test is selected and
    # runs green. The "selecting changes since last green run" line proves the
    # baseline path was taken (it never prints when strict hijacks the base).
    git(sp, "add", "-A")
    git_commit(sp, "commit-lockfile")  # re-establishes baseline on the prior run's green
    r = g.run("--since-green", "-n", "2", cwd=sp, env_extra=env)  # advance baseline, clean tree
    g.write("incrproj/test_alpha.py", "def test_a():\n    assert 1 + 0 == 1  # touched\n")
    git(sp, "add", "-A")
    git_commit(sp, "touch-alpha")
    r = g.run("--since-green", "--changed-strict", "-n", "2", "-v", cwd=sp, env_extra=env)
    check(
        "since-green: --changed-strict does not hijack the since-green base",
        r.returncode == 0
        and "selecting changes since last green run" in r.stderr
        and "test_a" in r.stdout,
        r.stderr[-300:] + r.stdout[-300:],
    )


def gate_incremental_dispatch_skip(g, args, binary):
    print("== incremental dispatch skip (--incremental) ==")
    sp = g.tmp / "incrdisp"
    g.write("incrdisp/mod_a.py", "def a():\n    return 1\n")
    g.write("incrdisp/mod_b.py", "def b():\n    return 2\n")
    g.write("incrdisp/test_a.py", "import mod_a\ndef test_a():\n    assert mod_a.a() == 1\n")
    g.write("incrdisp/test_b.py", "import mod_b\ndef test_b():\n    assert mod_b.b() == 2\n")
    env = {"PYTHONPATH": str(sp)}
    cov = ["--cov=.", "--cov-context=test", "--cov-report="]

    def run():
        # Wipe only the coverage data (SQLite; a stale one can lock under the
        # parallel combine); KEEP .rstest_cache so the index + outcomes persist.
        for stale in sp.glob(".coverage*"):
            stale.unlink()
        return g.run(
            "test_a.py", "test_b.py", "-n", "2", *cov, "--incremental", cwd=sp, env_extra=env
        )

    idx = sp / ".rstest_cache" / "coverage_index.json"

    # Run 1: no baseline -> everything runs, warming the index + green outcomes.
    r = run()
    check(
        "incremental: first run warms index + records green",
        r.returncode == 0 and idx.exists() and "2 passed" in r.stdout,
        f"rc={r.returncode} idx={idx.exists()} " + r.stdout[-200:],
    )

    # Run 2: nothing changed -> BOTH tests skipped as cached.
    r = run()
    check(
        "incremental: unchanged suite -> all cached",
        r.returncode == 0 and "2 of 2 test(s) unchanged" in r.stderr and "(2 cached)" in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Run 3: edit a dependency of ONE test -> only that test runs; the other is
    # cached (per-test granularity).
    g.write("incrdisp/mod_a.py", "def a():\n    return 1  # touched\n")
    r = run()
    check(
        "incremental: changed dependency reruns only the affected test",
        r.returncode == 0 and "1 of 2 test(s) unchanged" in r.stderr,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Run 4: nothing changed since the partial run -> BOTH cached again. This is
    # the carry-forward guard: the cached test's coverage was folded back into
    # the index covtool rewrote from only the test that ran.
    r = run()
    check(
        "incremental: carry-forward keeps cached tests skippable",
        r.returncode == 0 and "2 of 2 test(s) unchanged" in r.stderr,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Run 5: edit a test's OWN file (its dependency is unchanged) -> it reruns,
    # proving the test-file hash gate (test files aren't always in the index).
    g.write(
        "incrdisp/test_b.py",
        "import mod_b\ndef test_b():\n    assert mod_b.b() == 2  # touched\n",
    )
    r = run()
    check(
        "incremental: editing the test file reruns it",
        r.returncode == 0 and "1 of 2 test(s) unchanged" in r.stderr,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Run 6: break a dependency -> the affected test reruns and FAILS; it is
    # never skipped, and the failure surfaces.
    g.write("incrdisp/mod_a.py", "def a():\n    return 999  # broken\n")
    r = run()
    check(
        "incremental: a failing test is rerun, not skipped",
        r.returncode == 1 and "1 failed" in r.stdout and "test_a" in r.stdout,
        r.stderr[-200:] + r.stdout[-300:],
    )


def gate_incremental_guards(g, args, binary):
    print("== incremental guards (conftest / mutual-exclusion / no-cov) ==")
    sp = g.tmp / "incrguard"
    # A package under --cov=pkg, with tests and a conftest.py at the ROOT — both
    # OUTSIDE the coverage scope, so covtool never measures the conftest.
    g.write("incrguard/pkg/__init__.py", "")
    g.write("incrguard/pkg/mod_a.py", "def a():\n    return 1\n")
    g.write("incrguard/pkg/mod_b.py", "def b():\n    return 2\n")
    g.write(
        "incrguard/test_a.py",
        "from pkg import mod_a\ndef test_a():\n    assert mod_a.a() == 1\n",
    )
    g.write(
        "incrguard/test_b.py",
        "from pkg import mod_b\ndef test_b():\n    assert mod_b.b() == 2\n",
    )
    g.write(
        "incrguard/conftest.py",
        "import pytest\n@pytest.fixture(autouse=True)\ndef _f():\n    yield\n",
    )
    env = {"PYTHONPATH": str(sp)}
    cov = ["--cov=pkg", "--cov-context=test", "--cov-report="]

    def run(*extra, with_cov=True):
        for stale in sp.glob(".coverage*"):
            stale.unlink()
        flags = ["test_a.py", "test_b.py", "-n", "2", "--incremental", *extra]
        if with_cov:
            flags = ["test_a.py", "test_b.py", "-n", "2", *cov, "--incremental", *extra]
        return g.run(*flags, cwd=sp, env_extra=env)

    # Warm the index + green outcomes, then confirm both tests cache.
    run()
    r = run()
    check(
        "incremental: unchanged suite caches under scoped --cov=pkg",
        r.returncode == 0 and "2 of 2 test(s) unchanged" in r.stderr,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Finding 1: edit the ROOT conftest.py — invisible to --cov=pkg. Source is
    # byte-identical, yet the behavior-bearing conftest changed; skipping must be
    # busted (no cached passes), not a sticky false-green.
    g.write(
        "incrguard/conftest.py",
        "import pytest\n@pytest.fixture(autouse=True)\ndef _f():\n    yield  # edited\n",
    )
    r = run()
    check(
        "incremental: conftest edit outside cov scope busts the skip",
        r.returncode == 0
        and "unchanged since last green" not in r.stderr
        and "2 passed" in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )

    # Finding 3: --incremental and --since-green are mutually exclusive.
    r = g.run(
        "test_a.py",
        "test_b.py",
        "-n",
        "2",
        *cov,
        "--incremental",
        "--since-green",
        cwd=sp,
        env_extra=env,
    )
    check(
        "incremental: mutually exclusive with --since-green",
        "mutually exclusive" in r.stderr,
        r.stderr[-300:],
    )

    # Finding 2: --incremental WITHOUT --cov (index stays warm from above but is
    # never refreshed) must warn.
    r = run(with_cov=False)
    check(
        "incremental: warns when run without --cov",
        "without --cov" in r.stderr,
        r.stderr[-300:],
    )

    # Review finding 1: a NARROWED --cov=pkg leaves first-party source outside
    # the scope coverage-invisible, so skipping can go stale. The run must warn.
    # (The whole gate here runs under --cov=pkg, so the scoped-cov warning fires
    # on every `run()` above; assert it explicitly.)
    r = run()
    check(
        "incremental: warns on a scoped --cov (invisible first-party source)",
        "scoped --cov" in r.stderr,
        r.stderr[-300:],
    )
