"""e2e gate sections: plugins."""

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
