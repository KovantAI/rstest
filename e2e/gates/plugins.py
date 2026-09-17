"""e2e gate sections: plugins."""

import json
import os
import shutil
import subprocess
from pathlib import Path

from _harness import (
    BASE_DEPS,
    CRASH,
    CRASHLOOP,
    DOCTEST_MOD,
    MP_SPAWN,
    NODECRASH_CONFTEST,
    NODECRASH_TEST,
    NODEHOOKS_CONFTEST,
    NODEHOOKS_TEST,
    NODEONEARG_CONFTEST,
    WINDOWS,
    Gate,
    check,
    clear_hook_log,
    find_python,
    make_venv,
    read_hook_log,
    venv_bin,
)


def gate_multiprocessing_spawn_children(g, args, binary):
    print("== multiprocessing-spawn children ==")
    # spawn-mode children re-import the worker's __main__ as __mp_main__
    # (runpy, no package context): the worker entry must import absolutely,
    # guard main(), and keep its sys.path bootstrap idempotent. anyio's
    # to_process exercises the same protocol.
    g.write("mpspawn/test_mpspawn.py", MP_SPAWN)
    r = g.run("mpspawn", "-n", "2")
    check("mp-spawn under pool", "2 passed" in r.stdout, r.stdout[-300:])


def gate_crash_handling(g, args, binary):
    print("== crash handling ==")
    g.write("crash/test_crash.py", CRASH)
    r = g.run("crash", "-n", "2")
    check("crash run completes", "1 failed, 5 passed" in r.stdout, r.stdout[-200:])
    check("crash attributed", "test_killer" in r.stdout and "crashed while running" in r.stdout)
    check("crash respawn notice", "respawning" in r.stderr, r.stderr[-200:])
    check("crash exit 1", r.returncode == 1)
    g.write("crashloop/test_loop.py", CRASHLOOP)
    r = g.run("crashloop", "-n", "2", timeout=60)
    check("crash-loop terminates", r.returncode != 0 and "passed" in r.stdout, r.stdout[-200:])


def gate_pytest_randomly_real_plugin(g, args, binary):
    print("== pytest-randomly (real plugin) ==")
    # Isolated venv: pytest-randomly shuffles collection order, so installing
    # it in the shared venv would break the source-order / lazy-order checks.
    # Here we prove the real plugin consumes rstest's synthesized workerinput
    # seed at -n >= 2 instead of KeyError-ing (the bug this PR fixes).
    rnd_venv = Path(args.venv + "-randomly").resolve()
    make_venv(rnd_venv, extra_deps=["pytest-randomly"])
    gr = Gate(binary, rnd_venv)
    gr.write(
        "rnd/test_rnd.py",
        "def test_a(): pass\n"
        "def test_b(): pass\n"
        "def test_c(): pass\n"
        # pytest-randomly resolves --randomly-seed from workerinput per worker.
        # A missing key -> KeyError at configure (no pass); a plugin that
        # ignored our key -> resolved seed != our derivation. Asserting the
        # plugin's *resolved* option (not just the raw workerinput value)
        # proves it actually consumed the key we synthesize.
        "def test_seed_consumed(request):\n"
        "    resolved = request.config.getoption('randomly_seed')\n"
        "    wi = request.config.workerinput\n"
        "    assert resolved == (int(wi['testrun_uid'], 16) & 0xFFFFFFFF)\n",
    )
    # Pin the run uid so a second invocation derives the same seed: same uid
    # -> same seed -> same shuffle. Reproducibility, end to end through the
    # real plugin.
    uid = "abc123def456789"
    r = gr.run("rnd", "-n", "2", env_extra={"RSTEST_RUN_UID": uid})
    check(
        "randomly: consumes synthesized seed, no crash at -n 2",
        "4 passed" in r.stdout,
        r.stdout[-400:],
    )
    r = gr.run("rnd", "-n", "2", env_extra={"RSTEST_RUN_UID": uid})
    check("randomly: reproducible seed with pinned uid", "4 passed" in r.stdout, r.stdout[-400:])


def gate_pytest_rerunfailures_xdist_no_sock_port_(g, args, binary):
    print("== pytest-rerunfailures + xdist (no sock_port KeyError) ==")
    # rerunfailures+xdist reads workerinput["sock_port"], a key only an xdist
    # master sets; with no master rstest KeyError'd at -n>=2. rstest now drops
    # the plugin in pytest_cmdline_main and owns reruns. xdist reproduces it.
    rf_venv = Path(args.venv + "-rerunfailures").resolve()
    make_venv(rf_venv, extra_deps=["pytest-rerunfailures", "pytest-xdist"])
    grf = Gate(binary, rf_venv)
    grf.write(
        "rf/test_rf.py",
        "import pytest\n"
        "_calls = {}\n"
        "@pytest.mark.flaky(reruns=2)\n"
        "def test_recovers():\n"
        "    n = _calls.get('x', 0) + 1\n"
        "    _calls['x'] = n\n"
        "    assert n > 1\n"
        "def test_a(): assert True\n"
        "def test_b(): assert True\n",
    )
    r = grf.run("rf", "-n", "2", "--reruns", "2")
    check(
        "rerunfailures+xdist: no sock_port KeyError at -n 2",
        "sock_port" not in (r.stdout + r.stderr),
        (r.stdout + r.stderr)[-500:],
    )
    check(
        "rerunfailures+xdist: session completes, flaky recovered natively",
        r.returncode == 0 and "passed" in r.stdout and "failed" not in r.stdout,
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )
    # -n 0: no RSTEST_WORKER_ID, so the plugin is NOT neutralized and keeps its
    # native single-process behavior (no sock_port branch, no crash).
    r = grf.run("rf", "-n", "0")
    check(
        "rerunfailures: -n 0 keeps native plugin, no crash",
        r.returncode == 0 and "sock_port" not in (r.stdout + r.stderr),
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )


def gate_pytest_retry_xdist_server_port_self_prov(g, args, binary):
    print("== pytest-retry + xdist (server_port self-provision) ==")
    # pytest-retry gates master vs worker on xdist + numprocesses; rstest keeps
    # numprocesses visible so each worker self-provisions a ReportServer (master
    # branch) and never reads the sourceless workerinput["server_port"].
    rt_venv = Path(args.venv + "-retry").resolve()
    make_venv(rt_venv, extra_deps=["pytest-retry", "pytest-xdist"])
    grt = Gate(binary, rt_venv)
    grt.write(
        "rt/test_rt.py",
        "import pytest\n"
        "from pytest_retry import retry_plugin\n"
        "from pytest_retry.server import ReportServer, ClientReporter\n"
        "_a = {}\n"
        "def test_retry_recovers():\n"
        # Only pytest-retry's --retries can pass this: no rstest --reruns, no
        # @mark.flaky. Fails on attempt 1, passes on attempt 2.
        "    _a['x'] = _a.get('x', 0) + 1\n"
        "    assert _a['x'] >= 2\n"
        "def test_master_branch(request):\n"
        # Prove the plugin self-provisioned (master), not the server_port client
        # branch - the direct evidence that the KeyError path is never taken.
        "    rep = retry_plugin.retry_manager.reporter\n"
        "    assert isinstance(rep, ReportServer), type(rep).__name__\n"
        "    assert not isinstance(rep, ClientReporter)\n"
        "    assert request.config.getoption('numprocesses', False)\n"
        "def test_real_failure_survives_retries():\n"
        # Retry must not mask a genuine always-failure.
        "    assert False\n",
    )
    r = grt.run("rt", "-n", "2", "--retries", "2")
    check(
        "retry+xdist: no server_port KeyError at -n 2",
        "server_port" not in (r.stdout + r.stderr) and "KeyError" not in (r.stdout + r.stderr),
        (r.stdout + r.stderr)[-500:],
    )
    check(
        "retry+xdist: master branch taken, engine recovers, real failure survives",
        r.returncode == 1 and "1 failed, 2 passed" in r.stdout,
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )


def gate_interpreter_probe_cache_heals_after_deps(g, args, binary):
    print("== interpreter probe cache (heals after deps installed) ==")
    # Regression: a NEGATIVE probe (interpreter present but worker shim not
    # importable, e.g. msgpack missing) must NOT be cached. The cache keys on
    # binary mtime, unchanged by pip install, so a cached false would persist.
    bare = g.tmp / "bareenv"
    shutil.rmtree(bare, ignore_errors=True)
    subprocess.run([find_python(), "-m", "venv", str(bare)], check=True)  # no deps -> no msgpack
    barepy = venv_bin(bare, "python")
    probe_cache = g.tmp / "probecache"
    shutil.rmtree(probe_cache, ignore_errors=True)
    g.write("probe/test_p.py", "def test_ok(): assert True\n")
    # 1) msgpack absent: the interpreter runs but can't host a worker, so the
    #    probe is negative and (with the fix) is not written to the cache.
    r = g.run("probe", "--python", str(barepy), env_extra={"RSTEST_CACHE_DIR": str(probe_cache)})
    check(
        "probe: msgpack-less interpreter rejected",
        r.returncode != 0 and ("worker shim" in r.stderr or "no usable" in r.stderr.lower()),
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # 2) install the worker deps into the SAME interpreter (binary mtime
    #    unchanged), then rerun with the SAME cache dir: a stale cached negative
    #    would still reject it; the fix re-probes negatives so it now succeeds.
    subprocess.run([str(venv_bin(bare, "pip")), "install", "-q", *BASE_DEPS], check=True)
    r = g.run("probe", "--python", str(barepy), env_extra={"RSTEST_CACHE_DIR": str(probe_cache)})
    check(
        "probe: heals after deps installed (negative not cached)",
        r.returncode == 0 and "1 passed" in r.stdout,
        f"rc={r.returncode} " + r.stderr[-200:] + " || " + r.stdout[-200:],
    )


def gate_testnodedown_for_crashed_workers(g, args, binary):
    print("== testnodedown for crashed workers ==")
    g.write("nodecrash/conftest.py", NODECRASH_CONFTEST)
    g.write("nodecrash/test_crashy.py", NODECRASH_TEST)
    crash_log = g.tmp / "node_crash.log"
    clear_hook_log(crash_log)
    r = g.run("nodecrash", "-n", "2", env_extra={"NODE_HOOK_LOG": str(crash_log)})
    text = read_hook_log(crash_log)
    ups = {line.split(":", 1)[1] for line in text.splitlines() if line.startswith("up:")}
    downs = {line.split(":", 1)[1] for line in text.splitlines() if line.startswith("down:")}
    check(
        "every provisioned ident torn down (incl. crashed worker's)",
        ups and ups == downs,
        f"ups={sorted(ups)} downs={sorted(downs)}\n" + r.stdout[-200:],
    )
    check("crash still attributed", "crashed while running" in r.stdout, r.stdout[-300:])


def gate_xdist_master_side_hooks(g, args, binary):
    print("== xdist master-side hooks ==")
    g.write("nodehooks/conftest.py", NODEHOOKS_CONFTEST)
    g.write("nodehooks/test_node.py", NODEHOOKS_TEST)
    node_log = g.tmp / "node_hooks.log"
    clear_hook_log(node_log)
    r = g.run("nodehooks", "-n", "2", env_extra={"NODE_HOOK_LOG": str(node_log)})
    check("configure_node fills workerinput", "2 passed" in r.stdout, r.stdout[-300:])
    log_text = read_hook_log(node_log)
    check(
        "testnodedown fired per worker",
        "down:follower_gw0" in log_text and "down:follower_gw1" in log_text,
        log_text,
    )
    check("testnodeready fired", "ready:gw0" in log_text, log_text)


def gate_one_arg_pytest_testnodedown(g, args, binary):
    print("== one-arg pytest_testnodedown ==")
    g.write("nodeonearg/conftest.py", NODEONEARG_CONFTEST)
    # Two tests so both workers get real work (scheduling may still put both
    # on gw0, but every worker fires its own testnodedown regardless).
    g.write("nodeonearg/test_node.py", "def test_a(): assert True\n\n\ndef test_b(): assert True\n")
    oa_log = g.tmp / "node_onearg.log"
    clear_hook_log(oa_log)
    r = g.run("nodeonearg", "-n", "2", env_extra={"NODE_HOOK_LOG": str(oa_log)})
    oa_text = read_hook_log(oa_log)
    check("one-arg testnodedown: run not crashed", "2 passed" in r.stdout, r.stdout[-300:])
    check(
        "one-arg testnodedown fired (no error= TypeError)",
        "down:oa_gw0" in oa_text and "down:oa_gw1" in oa_text,
        oa_text + "\n" + r.stdout[-300:],
    )


def gate_doctest_modules(g, args, binary):
    print("== --doctest-modules ==")
    g.write("doctests/mymod.py", DOCTEST_MOD)
    g.write("doctests/test_real.py", "def test_plain(): assert True\n")
    r = g.run(".", "-n", "2", "--doctest-modules", cwd=g.tmp / "doctests")
    check(
        "doctest-modules pool counts",
        "1 failed, 2 passed" in r.stdout,
        r.stdout[-200:],
    )
    check(
        "doctest failure rendered",
        "Expected:" in r.stdout and "Got:" in r.stdout,
        r.stdout[-400:],
    )
    r = g.run(".", "-n", "0", "--doctest-modules", cwd=g.tmp / "doctests")
    check("doctest-modules -n 0", "1 failed, 2 passed" in r.stdout, r.stdout[-200:])


def gate_pytest_benchmark_autodisable(g, args, binary):
    print("== pytest-benchmark (auto-disables at -n>=2, measures at -n 0) ==")
    # Isolated venv: pytest-benchmark registers a `benchmark` fixture that would
    # collide with pytest-codspeed's, and it shuffles nothing, so keep it out of
    # the shared venv. Verdict under test: 🔶 -n 0 (top-50 #17). Auto-disable is
    # observable behaviorally — when the plugin sees the pool as xdist it skips
    # measurement AND does not save --benchmark-json; at -n 0 it measures and
    # writes the file. Asserting the artifact (not a terminal note rstest owns)
    # makes the auto-disable a hard check.
    bm_venv = Path(args.venv + "-benchmark").resolve()
    make_venv(bm_venv, extra_deps=["pytest-benchmark"])
    gb = Gate(binary, bm_venv)
    gb.write(
        "bm/test_bm.py",
        "def test_bench(benchmark):\n"
        "    result = benchmark(lambda: sum(range(100)))\n"
        "    assert result == 4950\n",
    )
    j2 = gb.tmp / "bench_n2.json"
    r = gb.run("bm", "-n", "2", f"--benchmark-json={j2}")
    check("benchmark: passes under pool", "1 passed" in r.stdout, r.stdout[-300:])
    check(
        "benchmark: auto-disabled at -n>=2 (no --benchmark-json written)",
        not j2.exists(),
        f"json unexpectedly written at -n 2: {j2}",
    )
    j0 = gb.tmp / "bench_n0.json"
    r = gb.run("bm", "-n", "0", f"--benchmark-json={j0}")
    check("benchmark: passes at -n 0", "1 passed" in r.stdout, r.stdout[-300:])
    n = len(json.loads(j0.read_text()).get("benchmarks", [])) if j0.exists() else 0
    check(
        "benchmark: measured + json written at -n 0",
        j0.exists() and n >= 1,
        f"exists={j0.exists()} benchmarks={n}",
    )


def gate_pytest_memray_limit_memory(g, args, binary):
    print("== pytest-memray (limit_memory enforced in parallel) ==")
    if WINDOWS:
        print("  skip: pytest-memray has no Windows support")
        return
    # limit_memory is enforced per test *process* by pytest-memray, independent
    # of the --memray report flag (the flag only drives the terminal summary,
    # which is the top-50 #44 🔶 -n 0 caveat). So the marker must fire the same
    # way at -n 0 and under the pool: each worker tracks its own allocations, so
    # the over-limit test fails identically in both — parallel parity on the
    # feature, not just coexistence.
    mr_venv = Path(args.venv + "-memray").resolve()
    make_venv(mr_venv, extra_deps=["pytest-memray"])
    gm = Gate(binary, mr_venv)
    gm.write(
        "mem/test_mem.py",
        "import pytest\n"
        "@pytest.mark.limit_memory('30 MB')\n"
        "def test_under():\n"
        "    b = bytearray(1 * 1024 * 1024)\n"
        "    assert len(b) == 1048576\n"
        "@pytest.mark.limit_memory('5 MB')\n"
        "def test_over():\n"
        "    b = bytearray(50 * 1024 * 1024)\n"
        "    assert len(b) == 52428800\n",
    )
    r0 = gm.run("mem", "-n", "0")
    check(
        "memray: limit_memory enforced at -n 0",
        "1 failed, 1 passed" in r0.stdout,
        r0.stdout[-300:],
    )
    check(
        "memray: over-limit test named with its cap",
        "test_over" in r0.stdout and "limited to 5" in r0.stdout,
        r0.stdout[-400:],
    )
    r2 = gm.run("mem", "-n", "2")
    check(
        "memray: same enforcement under the pool (parallel parity)",
        "1 failed, 1 passed" in r2.stdout,
        r2.stdout[-300:],
    )


def gate_pytest_codspeed_coexists(g, args, binary):
    print("== pytest-codspeed (fixture/marker coexist; --codspeed runs under pool) ==")
    # Isolated venv: codspeed also registers a `benchmark` fixture (would clash
    # with pytest-benchmark). Verdict under test: 🔶 -n 0 (top-50 #45). Without
    # --codspeed the benchmark fixture + @mark.benchmark just run the target once
    # and must pass under the pool and at -n 0 (coexistence); with --codspeed the
    # measurement pass (walltime simulation absent valgrind) must complete under
    # the pool without crashing.
    cs_venv = Path(args.venv + "-codspeed").resolve()
    make_venv(cs_venv, extra_deps=["pytest-codspeed"])
    gc = Gate(binary, cs_venv)
    gc.write(
        "cs/test_cs.py",
        "import pytest\n"
        "@pytest.mark.benchmark\n"
        "def test_marked():\n"
        "    assert sum(range(10)) == 45\n"
        "def test_fixture(benchmark):\n"
        "    assert benchmark(lambda: sum(range(10))) == 45\n",
    )
    r = gc.run("cs", "-n", "2")
    check("codspeed: fixture+marker coexist under pool", "2 passed" in r.stdout, r.stdout[-300:])
    r = gc.run("cs", "-n", "0")
    check("codspeed: 2 passed at -n 0", "2 passed" in r.stdout, r.stdout[-300:])
    r = gc.run("cs", "-n", "2", "--codspeed", timeout=180)
    check(
        "codspeed: --codspeed measurement completes under pool",
        "2 passed" in r.stdout,
        r.stdout[-300:],
    )


def _plugin_gate(binary, args, suffix, deps):
    """Make (or reuse) an isolated venv for one plugin and return a Gate."""
    venv = Path(args.venv + suffix).resolve()
    make_venv(venv, extra_deps=deps)
    return Gate(binary, venv)


# One passing + one failing test — the fixture several coexistence gates reuse.
_OK_BAD = "def test_ok():\n    assert True\ndef test_bad():\n    assert False\n"


def gate_pytest_subtests(g, args, binary):
    print("== pytest-subtests (sub-results ride the report hook) ==")
    # Each subtest emits its own report through the standard runtest hook, so a
    # failing subtest must surface as a distinct failure under the pool, keyed to
    # its worker — proving sub-reports survive the streamed merge.
    gs = _plugin_gate(binary, args, "-subtests", ["pytest-subtests"])
    gs.write(
        "st/test_st.py",
        "def test_all_pass(subtests):\n"
        "    for i in range(3):\n"
        "        with subtests.test(msg='ok', i=i):\n"
        "            assert i >= 0\n"
        "def test_one_sub_fails(subtests):\n"
        "    for i in range(3):\n"
        "        with subtests.test(msg='chk', i=i):\n"
        "            assert i != 1\n",
    )
    r = gs.run("st", "-n", "2")
    check(
        "subtests: clean + failing test both reported",
        "1 failed, 1 passed" in r.stdout,
        r.stdout[-300:],
    )
    check(
        "subtests: failing subtest attributed",
        "failed subtest" in r.stdout and "test_one_sub_fails" in r.stdout,
        r.stdout[-400:],
    )


def gate_pytest_check(g, args, binary):
    print("== pytest-check (soft multi-assert) ==")
    # A soft-assert test collects multiple failures in one test; all must survive
    # to the merged report under the pool (not just the first).
    gc = _plugin_gate(binary, args, "-check", ["pytest-check"])
    gc.write(
        "chk/test_chk.py",
        "import pytest_check as check\n"
        "def test_soft_multi():\n"
        "    check.equal(1, 2)\n"
        "    check.equal(3, 3)\n"
        "    check.is_true(False)\n"
        "def test_soft_clean():\n"
        "    check.equal(5, 5)\n",
    )
    r = gc.run("chk", "-n", "2")
    check("check: soft failures collected", "1 failed, 1 passed" in r.stdout, r.stdout[-300:])
    check(
        "check: both soft failures survive the merge",
        "Failed Checks: 2" in r.stdout,
        r.stdout[-400:],
    )


def gate_pytest_unordered(g, args, binary):
    print("== pytest-unordered (assertion helper) ==")
    gu = _plugin_gate(binary, args, "-unordered", ["pytest-unordered"])
    gu.write(
        "uo/test_uo.py",
        "from pytest_unordered import unordered\n"
        "def test_uo():\n"
        "    assert [1, 2, 3] == unordered([3, 1, 2])\n",
    )
    r = gu.run("uo", "-n", "2")
    check("unordered: helper works under pool", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_base_url(g, args, binary):
    print("== pytest-base-url (--base-url fixture) ==")
    gb = _plugin_gate(binary, args, "-baseurl", ["pytest-base-url"])
    gb.write(
        "bu/test_bu.py",
        "def test_bu(base_url):\n    assert base_url == 'https://example.test'\n",
    )
    r = gb.run("bu", "-n", "2", "--base-url", "https://example.test")
    check("base-url: fixture delivered to every worker", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_httpx(g, args, binary):
    print("== pytest-httpx (per-test httpx mock) ==")
    # httpx_mock is function-scoped; each test's mocked transport must be isolated
    # per worker (no shared master state).
    gh = _plugin_gate(binary, args, "-httpx", ["pytest-httpx", "httpx"])
    gh.write(
        "hx/test_hx.py",
        "import httpx\n"
        "def test_mock(httpx_mock):\n"
        "    httpx_mock.add_response(json={'ok': True})\n"
        "    assert httpx.get('https://x.test/api').json() == {'ok': True}\n",
    )
    r = gh.run("hx", "-n", "2")
    check("httpx: per-test mock works under pool", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_dotenv(g, args, binary):
    print("== pytest-dotenv (.env applied per worker) ==")
    # Every worker process must load the .env, so env-dependent tests pass on
    # whichever worker runs them.
    gd = _plugin_gate(binary, args, "-dotenv", ["pytest-dotenv"])
    gd.write("de/.env", "DOTENV_MARKER=hello123\n")
    gd.write("de/pytest.ini", "[pytest]\nenv_files = .env\n")
    gd.write(
        "de/test_env.py",
        "import os\n"
        "def test_a():\n    assert os.environ.get('DOTENV_MARKER') == 'hello123'\n"
        "def test_b():\n    assert os.environ.get('DOTENV_MARKER') == 'hello123'\n",
    )
    r = gd.run(".", "-n", "2", cwd=gd.tmp / "de")
    check("dotenv: .env loaded on every worker", "2 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_metadata(g, args, binary):
    print("== pytest-metadata (session metadata per worker) ==")
    gm = _plugin_gate(binary, args, "-metadata", ["pytest-metadata"])
    gm.write(
        "md/test_md.py",
        "def test_loaded(request):\n"
        "    assert request.config.pluginmanager.hasplugin('metadata')\n",
    )
    r = gm.run("md", "-n", "2")
    check("metadata: plugin active on workers", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_icdiff(g, args, binary):
    print("== pytest-icdiff (assertion-diff repr) ==")
    # icdiff rewrites the assertion diff into a side-by-side view; the rewrite
    # must reach the merged failure output from a worker.
    gi = _plugin_gate(binary, args, "-icdiff", ["pytest-icdiff"])
    gi.write(
        "ic/test_ic.py",
        "def test_diff():\n    assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}\n",
    )
    r = gi.run("ic", "-n", "2")
    check(
        "icdiff: side-by-side diff in worker failure",
        "1 failed" in r.stdout and "assert equals failed" in r.stdout,
        r.stdout[-400:],
    )


def gate_pytest_snapshot(g, args, binary):
    print("== pytest-snapshot (update at -n 0, assert under pool) ==")
    # Write snapshots single-worker (avoid same-file write races), then assert
    # them under the pool — the parallel-safe half of the workflow.
    gs = _plugin_gate(binary, args, "-snapshot", ["pytest-snapshot"])
    gs.write(
        "sn/test_sn.py",
        "def test_snap(snapshot):\n    snapshot.assert_match('hello world\\n', 'greeting.txt')\n",
    )
    r = gs.run(".", "-n", "0", "--snapshot-update", cwd=gs.tmp / "sn")
    check("snapshot: created at -n 0", "napshot" in r.stdout, r.stdout[-300:])
    r = gs.run(".", "-n", "2", cwd=gs.tmp / "sn")
    check("snapshot: asserts under pool", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_factoryboy(g, args, binary):
    print("== pytest-factoryboy (registered factory fixtures) ==")
    gf = _plugin_gate(binary, args, "-factoryboy", ["pytest-factoryboy", "factory_boy"])
    gf.write(
        "fb/test_fb.py",
        "import factory\n"
        "from pytest_factoryboy import register\n"
        "class User:\n"
        "    def __init__(self, name):\n        self.name = name\n"
        "class UserFactory(factory.Factory):\n"
        "    class Meta:\n        model = User\n"
        "    name = 'alice'\n"
        "register(UserFactory)\n"
        "def test_fb(user):\n    assert user.name == 'alice'\n",
    )
    r = gf.run("fb", "-n", "2")
    check("factoryboy: generated fixture on worker", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_allure(g, args, binary):
    print("== allure-pytest (per-test result files under pool) ==")
    # allure writes one <uuid>-result.json per test to --alluredir; each worker
    # writes its own, so the dir must hold one file per test after a pool run.
    ga = _plugin_gate(binary, args, "-allure", ["allure-pytest"])
    ga.write(
        "al/test_al.py",
        "def test_a1():\n    assert True\ndef test_a2():\n    assert True\n",
    )
    outdir = ga.tmp / "alluredir"
    shutil.rmtree(outdir, ignore_errors=True)
    r = ga.run("al", "-n", "2", f"--alluredir={outdir}")
    n = len(list(outdir.glob("*-result.json"))) if outdir.exists() else 0
    check(
        "allure: one result file per test written across workers",
        "2 passed" in r.stdout and n == 2,
        f"passed? {'2 passed' in r.stdout} result_files={n}",
    )


def gate_pytest_json_report_silent(g, args, binary):
    print("== pytest-json-report (silent at -n>=2, native --report-json) ==")
    # Report aggregators gate on the xdist master; under the pool there is none,
    # so no file is written (🔴 Silent, top-50 #16). At -n 0 the single session
    # writes it. Use rstest's native --report-json instead under the pool.
    gj = _plugin_gate(binary, args, "-jsonreport", ["pytest-json-report"])
    gj.write("jr/test_jr.py", "def test_x():\n    assert True\ndef test_y():\n    assert True\n")
    j2 = gj.tmp / "jr_n2.json"
    r = gj.run("jr", "-n", "2", "--json-report", f"--json-report-file={j2}")
    check(
        "json-report: silent at -n>=2 (no file)",
        "2 passed" in r.stdout and not j2.exists(),
        f"passed? {'2 passed' in r.stdout} file_exists={j2.exists()}",
    )
    j0 = gj.tmp / "jr_n0.json"
    r = gj.run("jr", "-n", "0", "--json-report", f"--json-report-file={j0}")
    check("json-report: written at -n 0", j0.exists(), f"file_exists={j0.exists()}")


def gate_pytest_json_ctrf_silent(g, args, binary):
    print("== pytest-json-ctrf (silent at -n>=2, native --report-json) ==")
    gj = _plugin_gate(binary, args, "-ctrf", ["pytest-json-ctrf"])
    gj.write("cf/test_cf.py", "def test_x():\n    assert True\ndef test_y():\n    assert True\n")
    j2 = gj.tmp / "ctrf_n2.json"
    r = gj.run("cf", "-n", "2", f"--ctrf={j2}")
    check(
        "json-ctrf: silent at -n>=2 (no file)",
        "2 passed" in r.stdout and not j2.exists(),
        f"passed? {'2 passed' in r.stdout} file_exists={j2.exists()}",
    )
    j0 = gj.tmp / "ctrf_n0.json"
    r = gj.run("cf", "-n", "0", f"--ctrf={j0}")
    check("json-ctrf: written at -n 0", j0.exists(), f"file_exists={j0.exists()}")


def gate_pytest_mypy(g, args, binary):
    print("== pytest-mypy (seeded stash path, no dead-master-path crash) ==")
    # pytest-mypy's worker branch reads workerinput["mypy_config_stash_serialized"]
    # (top-100 #97), a key only its xdist CONTROLLER sets. Under rstest every
    # process has workerinput but none is the controller, so the worker branch
    # read a key nobody set -> KeyError aborting configure at -n>=2 (same
    # dead-master-path class as random-order). rstest now seeds a unique per-worker
    # results-cache path; mypy runs lazily per worker (MypyResults.from_session).
    gm = _plugin_gate(binary, args, "-mypy", ["pytest-mypy"])
    # A clean file (mypy passes) and one with a return-type error (mypy fails);
    # both carry a real test so we also see the tests execute alongside the mypy
    # items under the pool.
    gm.write(
        "mp/test_good.py",
        "def add(a: int, b: int) -> int:\n    return a + b\n\n\n"
        "def test_add():\n    assert add(1, 2) == 3\n",
    )
    gm.write(
        "mp/test_bad.py",
        # Declared -> str but returns int: mypy reports an incompatible return.
        "def wrong(a: int) -> str:\n    return a\n\n\n"
        "def test_wrong_runs():\n    assert wrong(1) == 1\n",
    )
    r = gm.run("mp", "-n", "2", "--mypy", timeout=180)
    out = r.stdout + r.stderr
    # The dead-master-path signature is a KeyError on the un-seeded stash key and
    # a dead id-carrier worker. (A plain "KeyError" substring is too broad — the
    # mypy-status item's expected type-error failure renders pluggy source that
    # literally contains `except KeyError`.) Assert the SPECIFIC key is gone and
    # no worker died at collection.
    check(
        "mypy: no dead-master-path crash at -n>=2",
        "mypy_config_stash_serialized" not in out
        and "died before reporting" not in out
        and "<internalerror>" not in out.lower(),
        out[-600:],
    )
    check(
        "mypy: type error surfaced under the pool (mypy actually ran per worker)",
        r.returncode != 0 and ("error:" in out or "mypy" in out.lower()),
        f"rc={r.returncode} " + out[-500:],
    )
    # Parity: the same type error surfaces at -n 0 (single session, plugin's own
    # controller branch — the path rstest is standing in for under the pool).
    r0 = gm.run("mp", "-n", "0", "--mypy", timeout=180)
    out0 = r0.stdout + r0.stderr
    check(
        "mypy: same type error at -n 0 (behavior parity)",
        r0.returncode != 0 and ("error:" in out0 or "mypy" in out0.lower()),
        f"rc={r0.returncode} " + out0[-500:],
    )


def gate_report_aggregators_silent(g, args, binary):
    print("== report aggregators (no dead-master crash; silent at -n>=2) ==")
    # pytest-reportlog / -md / -nunit / -csv each write ONE whole-suite artifact.
    # Under the pool there is no master to aggregate: reportlog/nunit write
    # nothing (🔴 Silent), -md writes an empty "0 tests" report, and -csv writes a
    # racy per-worker file (⚠️ Caveat) — top-100 #81/#84/#91/#98. The hard,
    # verified property this gate protects across all four is that merely
    # installing + invoking them does NOT crash the worker (no KeyError /
    # traceback) at -n>=2, and the artifact still lands at -n 0. Use rstest's
    # native --report-json / --junitxml under the pool instead.
    cases = [
        ("reportlog", "pytest-reportlog", lambda f: [f"--report-log={f}"]),
        ("md", "pytest-md", lambda f: [f"--md={f}"]),
        ("nunit", "pytest-nunit", lambda f: [f"--nunit-xml={f}"]),
        ("csv", "pytest-csv", lambda f: [f"--csv={f}"]),
    ]
    for name, dep, flags in cases:
        gp = _plugin_gate(binary, args, "-agg-" + name, [dep])
        gp.write(
            "ag/test_ag.py",
            "def test_x():\n    assert True\ndef test_y():\n    assert True\n",
        )
        f2 = gp.tmp / f"{name}_n2.out"
        r = gp.run("ag", "-n", "2", *flags(f2), timeout=90)
        out = r.stdout + r.stderr
        check(
            f"{name}: no crash under the pool (session completes, no KeyError)",
            "2 passed" in r.stdout and "KeyError" not in out and "Traceback" not in out,
            out[-500:],
        )
        f0 = gp.tmp / f"{name}_n0.out"
        r0 = gp.run("ag", "-n", "0", *flags(f0), timeout=90)
        check(
            f"{name}: artifact written at -n 0",
            f0.exists(),
            f"file_exists={f0.exists()} " + (r0.stdout + r0.stderr)[-300:],
        )


def gate_silent_master_warning(g, args, binary):
    print("== silent-master plugin warning (-n>=2 + dark report flag) ==")
    # rstest warns before the run when a parallel run pairs with a plugin flag
    # whose artifact goes dark under the pool (aggregates on the absent xdist
    # master), pointing at the native parallel-safe path. Argv-driven — fires
    # whether or not the plugin is installed; here pytest-reportlog is installed
    # so the session itself also succeeds.
    gp = _plugin_gate(binary, args, "-agg-reportlog", ["pytest-reportlog"])
    gp.write("wn/test_wn.py", "def test_a():\n    assert True\n")
    rl = gp.tmp / "r.jsonl"
    r = gp.run("wn", "-n", "2", f"--report-log={rl}")
    out = r.stdout + r.stderr
    check(
        "warn: fires at -n>=2, names the plugin + native path",
        "warning:" in out and "pytest-reportlog" in out and "--report-json" in out,
        out[-400:],
    )
    check("warn: run still completes", "1 passed" in r.stdout, r.stdout[-200:])
    # Single-worker: the plugin's own master branch runs, so no warning.
    r0 = gp.run("wn", "-n", "0", f"--report-log={rl}")
    check(
        "warn: silent at -n 0 (no false positive)",
        "warning:" not in (r0.stdout + r0.stderr),
        (r0.stdout + r0.stderr)[-300:],
    )
    # rstest OWNS --junitxml (rendered from merged results) — never warn on it.
    rx = gp.tmp / "r.xml"
    r2 = gp.run("wn", "-n", "2", f"--junitxml={rx}")
    check(
        "warn: no false positive on rstest-owned --junitxml",
        "warning:" not in (r2.stdout + r2.stderr) and rx.exists(),
        f"file_exists={rx.exists()} " + (r2.stdout + r2.stderr)[-300:],
    )


def gate_pytest_random_order(g, args, binary):
    print("== pytest-random-order (seeded workerinput, no dead-master-path crash) ==")
    # pytest-random-order's pytest_configure reads workerinput["random_order_seed"]
    # unconditionally when workerinput exists (even with reordering off, its
    # default), so merely installing it KeyError'd every -n>=2 run. rstest now
    # seeds that key (shared across workers), closing the dead-master-path.
    gr = _plugin_gate(binary, args, "-randomorder", ["pytest-random-order"])
    gr.write(
        "ro/test_ro.py",
        "def test_1():\n    assert True\n"
        "def test_2():\n    assert True\n"
        "def test_3():\n    assert True\n"
        "def test_4():\n    assert True\n",
    )
    # Installed-but-not-enabled (the default): must no longer crash under the pool.
    r = gr.run("ro", "-n", "2")
    check(
        "random-order: installed plugin no longer KeyErrors at -n>=2",
        "4 passed" in r.stdout and "random_order_seed" not in (r.stdout + r.stderr),
        (r.stdout + r.stderr)[-400:],
    )
    # Explicitly enabled: all workers agree on the seeded value, so the shuffled
    # collection hashes match and the run completes.
    r = gr.run("ro", "-n", "2", "--random-order")
    check(
        "random-order: --random-order runs under the pool (workers agree on seed)",
        "4 passed" in r.stdout,
        (r.stdout + r.stderr)[-400:],
    )
    r = gr.run("ro", "-n", "0", "--random-order")
    check("random-order: honored at -n 0", "4 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_httpserver(g, args, binary):
    print("== pytest-httpserver (per-test local server) ==")
    gh = _plugin_gate(binary, args, "-httpserver", ["pytest-httpserver"])
    gh.write(
        "hs/test_hs.py",
        "import json, urllib.request\n"
        "def test_srv(httpserver):\n"
        "    httpserver.expect_request('/ping').respond_with_json({'pong': True})\n"
        "    url = httpserver.url_for('/ping')\n"
        "    assert json.load(urllib.request.urlopen(url)) == {'pong': True}\n",
    )
    r = gh.run("hs", "-n", "2")
    check("httpserver: per-test server on its own port", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_bdd(g, args, binary):
    print("== pytest-bdd (.feature-generated items per worker) ==")
    gb = _plugin_gate(binary, args, "-bdd", ["pytest-bdd"])
    gb.write(
        "bd/add.feature",
        "Feature: calc\n"
        "  Scenario: add two numbers\n"
        "    Given I have 1 and 2\n"
        "    When I add them\n"
        "    Then the result is 3\n",
    )
    gb.write(
        "bd/test_bdd.py",
        "from pytest_bdd import scenario, given, when, then\n"
        "@scenario('add.feature', 'add two numbers')\n"
        "def test_add():\n    pass\n"
        "@given('I have 1 and 2', target_fixture='nums')\n"
        "def nums():\n    return {'a': 1, 'b': 2, 'r': None}\n"
        "@when('I add them')\n"
        "def add(nums):\n    nums['r'] = nums['a'] + nums['b']\n"
        "@then('the result is 3')\n"
        "def check_it(nums):\n    assert nums['r'] == 3\n",
    )
    r = gb.run(".", "-n", "2", cwd=gb.tmp / "bd")
    check("bdd: scenario runs under pool", "1 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_dependency(g, args, binary):
    print("== pytest-dependency (honored at -n 0; cross-worker caveat) ==")
    # Cross-test dependencies resolve within a single session: at -n 0 a failed
    # dependency skips its dependents. At -n >= 2 dependents may land on another
    # worker and not see the result — use -n 0 or --dist loadscope (the ⚠️ #26
    # caveat). Assert the -n 0 semantics and that the pool run still completes.
    gd = _plugin_gate(binary, args, "-dependency", ["pytest-dependency"])
    gd.write(
        "dp/test_dp.py",
        "import pytest\n"
        "@pytest.mark.dependency()\n"
        "def test_a():\n    assert False\n"
        "@pytest.mark.dependency(depends=['test_a'])\n"
        "def test_b():\n    assert True\n",
    )
    r = gd.run("dp", "-n", "0")
    check(
        "dependency: dependent skipped when dep fails (-n 0)",
        "1 failed, 1 skipped" in r.stdout,
        r.stdout[-300:],
    )
    r = gd.run("dp", "-n", "2")
    check("dependency: pool run completes (no crash)", "failed" in r.stdout, r.stdout[-300:])


def gate_pytest_ordering(g, args, binary):
    print("== pytest-ordering (order honored within a worker) ==")
    go = _plugin_gate(binary, args, "-ordering", ["pytest-ordering"])
    go.write(
        "od/test_od.py",
        "import pytest\n"
        "_order = []\n"
        "@pytest.mark.run(order=2)\n"
        "def test_second():\n    _order.append('s')\n    assert _order == ['f', 's']\n"
        "@pytest.mark.run(order=1)\n"
        "def test_first():\n    _order.append('f')\n    assert _order == ['f']\n",
    )
    r = go.run("od", "-n", "0")
    check("ordering: @mark.run order honored at -n 0", "2 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_split(g, args, binary):
    print("== pytest-split (group selection honored; native --shard) ==")
    # pytest-split's --splits/--group is deselection; it is honored under the
    # pool (a subset runs). rstest's native sharding is --shard K/N (top-50 #14).
    gsp = _plugin_gate(binary, args, "-split", ["pytest-split"])
    gsp.write(
        "sp/test_sp.py",
        "def test_1():\n    assert True\ndef test_2():\n    assert True\n"
        "def test_3():\n    assert True\ndef test_4():\n    assert True\n",
    )
    r = gsp.run("sp", "-n", "2", "--splits", "2", "--group", "1")
    check(
        "split: group selection runs a subset under the pool",
        "2 passed" in r.stdout,
        r.stdout[-300:],
    )


def gate_pytest_testmon(g, args, binary):
    print("== pytest-testmon (single-worker; native incremental) ==")
    # testmon's shared .testmondata is not concurrency-safe, so it is an -n 0
    # feature (top-50 #37); rstest has native --changed/coverage selection. Verify
    # it runs single-worker without error.
    gt = _plugin_gate(binary, args, "-testmon", ["pytest-testmon"])
    gt.write("tm/test_tm.py", "def test_1():\n    assert True\ndef test_2():\n    assert True\n")
    r = gt.run("tm", "-n", "0", "--testmon")
    check("testmon: runs at -n 0", "2 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_instafail(g, args, binary):
    print("== pytest-instafail (coexists; rstest streams failures natively) ==")
    gi = _plugin_gate(binary, args, "-instafail", ["pytest-instafail"])
    gi.write("if/test_if.py", _OK_BAD)
    r = gi.run("if", "-n", "2", "--instafail")
    check(
        "instafail: coexists under the pool",
        "1 failed, 1 passed" in r.stdout,
        r.stdout[-300:],
    )


def gate_pytest_durations(g, args, binary):
    print("== pytest-durations (coexists; native --durations/--doctor) ==")
    gd = _plugin_gate(binary, args, "-durations", ["pytest-durations"])
    gd.write("du/test_du.py", "def test_1():\n    assert True\ndef test_2():\n    assert True\n")
    r = gd.run("du", "-n", "2")
    check("durations: coexists under the pool", "2 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_custom_exit_code(g, args, binary):
    print("== pytest-custom-exit-code (rstest owns the exit status) ==")
    # rstest computes the process exit status from merged results, so the plugin's
    # per-worker exit hook is overridden: --suppress-tests-failed-exit-code cannot
    # turn a failing run green (the ⚠️ #29 caveat).
    gc = _plugin_gate(binary, args, "-customexit", ["pytest-custom-exit-code"])
    gc.write("ce/test_ce.py", _OK_BAD)
    r = gc.run("ce", "-n", "2", "--suppress-tests-failed-exit-code")
    check(
        "custom-exit-code: rstest exit status wins over the plugin",
        "1 failed, 1 passed" in r.stdout and r.returncode == 1,
        f"rc={r.returncode} " + r.stdout[-300:],
    )


def gate_pytest_timeouts(g, args, binary):
    print("== pytest-timeouts (coexists; native --timeout/--worker-timeout) ==")
    gt = _plugin_gate(binary, args, "-timeouts", ["pytest-timeouts"])
    gt.write("to/test_to.py", "def test_1():\n    assert True\ndef test_2():\n    assert True\n")
    r = gt.run("to", "-n", "2")
    check("timeouts: coexists under the pool", "2 passed" in r.stdout, r.stdout[-300:])


def gate_pytest_gh_annotate(g, args, binary):
    print("== pytest-github-actions-annotate-failures (native --output github) ==")
    # The plugin coexists under the pool; rstest emits GitHub annotations natively
    # via --output github (top-50 #41), so that is the recommended path.
    gg = _plugin_gate(binary, args, "-ghannotate", ["pytest-github-actions-annotate-failures"])
    gg.write("gh/test_gh.py", _OK_BAD)
    r = gg.run("gh", "-n", "2", env_extra={"GITHUB_ACTIONS": "true"})
    check(
        "gh-annotate: plugin coexists under the pool",
        "1 failed, 1 passed" in r.stdout,
        r.stdout[-300:],
    )
    r = gg.run("gh", "-n", "2", "--output", "github")
    check(
        "gh-annotate: native --output github emits ::error",
        "::error" in r.stdout and "test_bad" in r.stdout,
        r.stdout[-400:],
    )


# --- Service-backed gates -------------------------------------------------
# These need an external resource (a postgres install, a playwright browser, the
# Home Assistant test stack) that the default offline gate run and most dev
# machines don't have. They are opt-in via RSTEST_GATE_SERVICES=1 (the CI
# `plugin-services` job sets it and provisions the resources) and self-skip
# otherwise, so a plain `python e2e/gate.py` never downloads a 95 MB browser or
# depends on postgres being installed.


def _services_enabled():
    return os.environ.get("RSTEST_GATE_SERVICES") == "1"


def gate_pytest_postgresql(g, args, binary):
    print("== pytest-postgresql (per-worker DB instance) ==")
    if not _services_enabled():
        print("  skip: set RSTEST_GATE_SERVICES=1 to run (needs a postgres install)")
        return
    if not (shutil.which("initdb") and shutil.which("pg_ctl")):
        print("  skip: postgres binaries (initdb/pg_ctl) not on PATH")
        return
    # pytest-postgresql's postgresql_proc spawns its own server on a free port
    # per process, so each worker gets an isolated instance — no shared master.
    gp = _plugin_gate(binary, args, "-postgresql", ["pytest-postgresql", "psycopg[binary]"])
    gp.write(
        "pg/test_pg.py",
        "def test_one(postgresql):\n"
        "    cur = postgresql.cursor()\n"
        "    cur.execute('SELECT 1')\n"
        "    assert cur.fetchone()[0] == 1\n"
        "def test_two(postgresql):\n"
        "    cur = postgresql.cursor()\n"
        "    cur.execute('CREATE TABLE t (id int); INSERT INTO t VALUES (7)')\n"
        "    cur.execute('SELECT id FROM t')\n"
        "    assert cur.fetchone()[0] == 7\n",
    )
    r = gp.run("pg", "-n", "2", timeout=180)
    check(
        "postgresql: per-worker instance works under the pool",
        "2 passed" in r.stdout,
        r.stdout[-400:],
    )


def gate_pytest_playwright(g, args, binary):
    print("== pytest-playwright (per-worker browser context) ==")
    if not _services_enabled():
        print("  skip: set RSTEST_GATE_SERVICES=1 to run (needs a playwright browser)")
        return
    gp = _plugin_gate(binary, args, "-playwright", ["pytest-playwright"])
    # Provision the chromium build into the venv (idempotent; cached after the
    # first download). Skip the gate if provisioning fails (offline runner).
    try:
        subprocess.run(
            [str(venv_bin(gp.venv, "playwright")), "install", "chromium"],
            check=True,
            capture_output=True,
            timeout=300,
        )
    except Exception as exc:  # any provisioning failure -> skip
        print(f"  skip: could not install chromium ({exc})")
        return
    gp.write(
        "pw/test_pw.py",
        "def test_a(page):\n"
        "    page.set_content('<h1>hello</h1>')\n"
        "    assert page.text_content('h1') == 'hello'\n"
        "def test_b(page):\n"
        "    page.set_content(\"<div id='x'>42</div>\")\n"
        "    assert page.text_content('#x') == '42'\n",
    )
    r = gp.run("pw", "-n", "2", timeout=180)
    check(
        "playwright: per-worker browser context under the pool",
        "2 passed" in r.stdout,
        r.stdout[-400:],
    )


def gate_pytest_homeassistant(g, args, binary):
    print("== pytest-homeassistant-custom-component (per-worker hass fixture) ==")
    if not _services_enabled():
        print("  skip: set RSTEST_GATE_SERVICES=1 to run (heavy Home Assistant deps)")
        return
    gh = _plugin_gate(binary, args, "-homeassistant", ["pytest-homeassistant-custom-component"])
    gh.write("ha/pytest.ini", "[pytest]\nasyncio_mode = auto\n")
    gh.write(
        "ha/test_ha.py",
        "async def test_hass_a(hass, enable_custom_integrations):\n"
        "    assert hass is not None\n"
        "    assert hass.states is not None\n"
        "async def test_hass_b(hass, enable_custom_integrations):\n"
        "    hass.states.async_set('sensor.x', '42')\n"
        "    assert hass.states.get('sensor.x').state == '42'\n",
    )
    r = gh.run(".", "-n", "2", cwd=gh.tmp / "ha", timeout=300)
    check(
        "homeassistant: per-worker hass fixture under the pool",
        "2 passed" in r.stdout,
        r.stdout[-400:],
    )
