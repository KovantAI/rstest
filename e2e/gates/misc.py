"""e2e gate sections: misc."""

from _harness import BASIC, check


def gate_basics(g, args, binary):
    print("== basics ==")
    g.write("basic/test_basic.py", BASIC)
    r = g.run("basic/test_basic.py", "-n", "2")
    check("parallel counts", "2 failed, 2 passed" in r.stdout, r.stdout[-200:])
    check("parallel exit 1", r.returncode == 1)
    check("header line", r.stdout.startswith("rstest "), r.stdout[:80])
    r = g.run("basic/test_basic.py", "-n", "0", "-k", "passes")
    check("-n 0 exact mode + -k", "2 passed" in r.stdout and "pytest-exact" in r.stdout)
    r = g.run("basic/test_basic.py", "--co", "-q")
    check("--co passthrough", "test_basic.py::test_passes" in r.stdout)
    r = g.run("basic/test_basic.py", "-n", "2", "-v")
    check("-v verbose lines", "::test_passes PASSED" in r.stdout, r.stdout[-300:])
    check("-v worker attribution", "[gw" in r.stdout, r.stdout[-300:])
    check("failure header attribution", "FAILED [gw" in r.stdout, r.stdout[-300:])
    r = g.run("basic/test_basic.py", "-n", "0", "-v")
    check("-n 0 has no worker prefixes", "[gw" not in r.stdout, r.stdout[-200:])
    g.write("empty/.keep", "")
    r = g.run("empty")
    check("no tests exit 5", r.returncode in (4, 5), f"rc={r.returncode}")


def gate_collection_error_semantics(g, args, binary):
    print("== collection error semantics ==")
    g.write("broken/test_broken.py", "import nonexistent_module_xyz\n")
    r = g.run("broken/test_broken.py", "-n", "0")
    check("collect error aborts (exit 2)", r.returncode == 2, f"rc={r.returncode}")
    g.write("broken/test_fine.py", "def test_ok(): assert True\n")
    r = g.run(".", "-n", "2", cwd=g.tmp / "broken")
    check(
        "pool: collect error aborts, runs nothing",
        r.returncode == 2 and " passed" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )


def gate_worker_identity_fixtures(g, args, binary):
    print("== worker identity fixtures (worker_id / testrun_uid) ==")
    # The gate venv has NO pytest-xdist installed, so these fixtures resolve
    # only because rstest ships them natively. This is the migrate-off-xdist
    # landmine: a suite that drops pytest-xdist from its config keeps
    # `def test(worker_id)` working.
    g.write(
        "widfix/test_wid.py",
        "def test_worker_id(worker_id):\n"
        "    assert worker_id == 'master' or worker_id.startswith('gw')\n"
        "def test_testrun_uid(testrun_uid):\n"
        "    assert isinstance(testrun_uid, str) and testrun_uid\n",
    )
    r = g.run("widfix/test_wid.py", "-n", "0")
    check("fixtures resolve at -n 0", "2 passed" in r.stdout, r.stdout[-300:])
    r = g.run("widfix/test_wid.py", "-n", "2")
    check("fixtures resolve under the pool", "2 passed" in r.stdout, r.stdout[-300:])
    # Pin the -n 0 value: single-worker mode has no worker identity, so
    # worker_id is "master" (xdist parity).
    g.write(
        "widfix/test_master.py",
        "def test_master(worker_id):\n    assert worker_id == 'master'\n",
    )
    r = g.run("widfix/test_master.py", "-n", "0")
    check("worker_id is 'master' at -n 0", "1 passed" in r.stdout, r.stdout[-300:])
