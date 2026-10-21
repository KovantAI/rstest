"""e2e gate sections: misc."""

import json

from _harness import BASIC, check


def gate_basics(g, args, binary):
    print("== basics ==")
    g.write("basic/test_basic.py", BASIC)
    r = g.run("basic/test_basic.py", "-n", "2")
    check("parallel counts", "2 failed, 2 passed" in r.stdout, r.stdout[-200:])
    check("parallel exit 1", r.returncode == 1)
    check("header line", r.stdout.startswith("rstest "), r.stdout[:80])
    r = g.run("basic/test_basic.py", "-n", "0", "-k", "passes")
    # Byte-exact mode prints pytest's own terminal output (its session header).
    check("-n 0 exact mode + -k", "2 passed" in r.stdout and "test session starts" in r.stdout)
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
    # --reruns at -n 0 runs a one-worker pool (workerinput is built); still
    # single-worker mode, so still "master".
    r = g.run("widfix/test_master.py", "-n", "0", "--reruns", "1")
    check("worker_id is 'master' at -n 0 --reruns", "1 passed" in r.stdout, r.stdout[-300:])


def gate_lone_surrogate_in_report(g, args, binary):
    # R17 (B1): a failure whose text holds a lone surrogate (a non-UTF-8 path
    # through os.fsdecode) has no UTF-8 encoding. It used to crash the worker's
    # msgpack encoder: INTERNALERROR, exit 3, and the next test never ran.
    # Decode with surrogateescape explicitly: os.fsdecode uses it on POSIX, but
    # on Windows its handler is surrogatepass and b"\xff" raises instead.
    print("== lone surrogate in a report ==")
    g.write(
        "surrogate/test_sur.py",
        "def test_a(): pass\n"
        "def test_bad():\n"
        '    raise FileNotFoundError(b"bad\\xff.txt".decode("utf-8", "surrogateescape"))\n'
        "def test_c(): pass\n",
    )
    for n in ("0", "2"):
        r = g.run("surrogate", "-n", n, "-p", "no:cacheprovider")
        out = r.stdout + r.stderr
        check(
            f"surrogate -n {n}: every test reported, exit 1",
            "1 failed, 2 passed" in r.stdout and r.returncode == 1,
            f"rc={r.returncode} " + out[-300:],
        )
        check(
            f"surrogate -n {n}: no worker crash",
            "INTERNALERROR" not in out and "surrogates not allowed" not in out,
            out[-300:],
        )
        if n != "0":  # -n 0 prints pytest's own output: the raw bytes
            check(
                f"surrogate -n {n}: escaped text shown",
                "bad\\udcff.txt" in r.stdout,
                r.stdout[-300:],
            )


def gate_failures_by_cause(g, args, binary):
    print("== failures by cause ==")
    # Three tests fail through one shared helper with the same exception (the
    # port differs: numbers are masked); one fails on its own assertion.
    g.write(
        "bycause/app/db.py",
        "def connect(port):\n"
        "    raise ConnectionRefusedError(111, f'Connection refused: port {port}')\n",
    )
    g.write("bycause/app/__init__.py", "")
    g.write(
        "bycause/test_db.py",
        "from app.db import connect\n\n"
        "def test_users(): connect(5432)\n\n"
        "def test_orders(): connect(5433)\n\n"
        "def test_items(): connect(5434)\n\n"
        "def test_math(): assert 1 + 1 == 3\n",
    )
    bdir = g.tmp / "bycause"
    rj = g.tmp / "bycause.json"
    r = g.run(".", "-n", "2", "--report-json", str(rj), cwd=bdir)
    out = r.stdout
    check(
        "failures by cause: one block naming the shared exception and frame",
        r.returncode == 1
        and "=========== failures by cause ===========" in out
        and "3 tests: ConnectionRefusedError: [Errno 111] Connection refused: port" in out
        and "  at app/db.py:2" in out
        and "test_math" not in out.split("failures by cause")[1].split("failed")[0],
        out[-700:],
    )
    doc = json.loads(rj.read_text(encoding="utf-8")) if rj.exists() else {}
    groups = doc.get("failure_groups", [])
    check(
        "failures by cause: report-json failure_groups",
        len(groups) == 1
        and groups[0]["exception"] == "ConnectionRefusedError"
        and groups[0]["at"] == "app/db.py:2"
        and groups[0]["count"] == 3
        and groups[0]["nodeids"]
        == ["test_db.py::test_items", "test_db.py::test_orders", "test_db.py::test_users"],
        str(groups)[:400],
    )
    r = g.run(".", "-n", "2", "--output", "github", cwd=bdir)
    errors = [ln for ln in r.stdout.splitlines() if ln.startswith("::error ")]
    check(
        "failures by cause: github gets one grouped annotation plus the single",
        len(errors) == 2
        and errors[0].startswith("::error file=app/db.py,title=3 tests%2C one cause,line=2::")
        and "test_db.py%3A%3Atest_math" in errors[1],
        "\n".join(e[:160] for e in errors),
    )
