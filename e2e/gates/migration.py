"""e2e gate sections: pytest / pytest-xdist migrator (MG-* scenarios).

A team lead swaps `pytest -n auto` for `rstest` on an existing, configured
suite (real `addopts`, conftest hooks, plugins), runs the readiness tools, and
expects every result to stay identical. The gate venv has no pytest-xdist, so
the oracle is plain `python -m pytest` run at gate time, or the documented
xdist behaviour. Checks tagged `known_bug=True` pin current failures: they
xfail today and turn the gate red once the bug is fixed, so the marker gets
dropped and the check becomes a regression guard.
"""

import json
import os
import re
import subprocess
import xml.etree.ElementTree as ET

from _harness import REPO, check, venv_bin


def _out(r):
    return r.stdout + r.stderr


def _last(text):
    lines = [ln for ln in text.strip().splitlines() if ln.strip()]
    return lines[-1] if lines else ""


def _counts(line):
    """Parse a pytest summary line ('== 1 failed, 2 passed in 0.1s ==') into
    {'failed': 1, 'passed': 2}. Plural nouns are folded to singular so a
    wording-only difference (X6, covered by EV-15) does not mask parity."""
    body = re.sub(r"\s+in [\d.]+s.*$", "", line.strip().strip("=").strip())
    out = {}
    for part in body.split(","):
        m = re.match(r"\s*(\d+) (.+?)\s*$", part)
        if m:
            key = re.sub(r"\b(error|warning|subtest)s\b", r"\1", m.group(2))
            out[key] = int(m.group(1))
    return out


def _junit(path):
    """{classname::name: sorted outcome tags} from a JUnit file; None if the
    file is missing (a usage error writes none)."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError):
        return None
    res = {}
    for tc in root.iter("testcase"):
        tags = sorted(c.tag for c in tc if c.tag in ("failure", "error", "skipped"))
        res[f"{tc.get('classname')}::{tc.get('name')}"] = tags or ["passed"]
    return res


def _pytest(g, cwd, *args, env_extra=None):
    """The oracle: plain pytest from the gate venv."""
    env = dict(os.environ)
    env.pop("PYTEST_ADDOPTS", None)
    env.update(env_extra or {})
    return subprocess.run(
        [str(venv_bin(g.venv, "python")), "-m", "pytest", "-p", "no:cacheprovider", *args],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=120,
    )


def _logs(base):
    """Rows from the per-process JSON-lines logs written by the fixtures'
    `_log` helper (one file per process: cross-process appends tear)."""
    rows = []
    for p in base.parent.glob(base.name + ".*"):
        for ln in p.read_text(encoding="utf-8").splitlines():
            if ln.strip():
                rows.append(json.loads(ln))
    return rows


def _clear(base):
    for p in base.parent.glob(base.name + ".*"):
        p.unlink()


# Fixture-side logger: one JSON line per event, one file per process.
_LOG = (
    "import json, os, time\n\n"
    "def _log(**kw):\n"
    "    kw.setdefault('w', os.environ.get('RSTEST_WORKER_ID') or 'main')\n"
    "    kw.setdefault('pid', os.getpid())\n"
    "    kw.setdefault('t', time.time())\n"
    "    with open(os.environ['MG_LOG'] + '.' + str(os.getpid()), 'a') as f:\n"
    "        f.write(json.dumps(kw) + '\\n')\n"
)


# ---------------------------------------------------------------- MG-01

_SUITES = {
    "cfg_custom_names": {
        "pytest.ini": "[pytest]\npython_files = check_*.py\n"
        "python_classes = Suite*\npython_functions = verify_*\n",
        "check_a.py": "def verify_one():\n    pass\n\ndef verify_two():\n    assert 0\n\n"
        "def test_not_collected():\n    assert 0\n",
        "check_b.py": "class SuiteX:\n    def verify_m(self):\n        pass\n\n"
        "class TestIgnored:\n    def verify_n(self):\n        assert 0\n",
        "test_not_matched.py": "def verify_x():\n    assert 0\n",
    },
    "cfg_strict": {
        "pyproject.toml": "[tool.pytest.ini_options]\n"
        'addopts = "--strict-markers"\n'
        'filterwarnings = ["error"]\n'
        "xfail_strict = true\n"
        'markers = ["slow: slow tests"]\n',
        "test_s.py": "import warnings, pytest\n\n"
        "@pytest.mark.slow\ndef test_marked():\n    pass\n\n"
        "def test_warns():\n    warnings.warn('old', DeprecationWarning)\n\n"
        "@pytest.mark.xfail\ndef test_xpass_strict():\n    pass\n\n"
        "@pytest.mark.xfail\ndef test_xfail():\n    assert 0\n",
    },
    "cfg_strict_markers": {
        "pytest.ini": "[pytest]\naddopts = --strict-markers\nmarkers =\n    slow: slow\n",
        "test_ok.py": "import pytest\n\n@pytest.mark.slow\ndef test_ok():\n    pass\n",
        "test_unknown_mark.py": "import pytest\n\n@pytest.mark.typo\ndef test_t():\n    pass\n",
    },
    "cfg_required": {
        "pytest.ini": "[pytest]\nrequired_plugins = pytest-mg-nonexistent\n",
        "test_r.py": "def test_r():\n    pass\n",
    },
    "cfg_minversion": {
        "pytest.ini": "[pytest]\nminversion = 99.0\n",
        "test_r.py": "def test_r():\n    pass\n",
    },
    "importlib_dupes": {
        "pytest.ini": "[pytest]\naddopts = --import-mode=importlib\n",
        "a/test_same.py": "def test_x():\n    pass\n",
        "b/test_same.py": "def test_x():\n    assert 0\n",
    },
    "hooks_modifyitems": {
        "conftest.py": "import pytest\n\n"
        "def pytest_collection_modifyitems(config, items):\n"
        "    for it in items:\n"
        "        if 'skipme' in it.name:\n"
        "            it.add_marker(pytest.mark.skip(reason='hook'))\n"
        "        if 'xfailme' in it.name:\n"
        "            it.add_marker(pytest.mark.xfail(reason='hook', strict=True))\n",
        "test_h.py": "def test_plain():\n    pass\n\n"
        "def test_skipme():\n    assert 0\n\ndef test_xfailme():\n    assert 0\n",
    },
    "hooks_deselect": {
        "conftest.py": "def pytest_collection_modifyitems(config, items):\n"
        "    drop = [it for it in items if 'deselectme' in it.name]\n"
        "    config.hook.pytest_deselected(items=drop)\n"
        "    items[:] = [it for it in items if it not in drop]\n",
        "test_d.py": "def test_plain():\n    pass\n\ndef test_deselectme():\n    assert 0\n",
    },
    "ids_weird": {
        "test_ids.py": "import pytest\n\n"
        "@pytest.mark.parametrize(\n"
        "    'v', ['a::b', 'x[1]', 'p/q', 'with space', '\\u00fcn\\u00efc\\u00f6de']\n"
        ")\n"
        "def test_v(v):\n    assert v != 'p/q'\n\n"
        "@pytest.mark.parametrize('v', [1, 2], ids=['id::colon', 'id [bracket]'])\n"
        "def test_ids(v):\n    pass\n",
    },
    "doctest": {
        "pytest.ini": "[pytest]\naddopts = --doctest-modules --doctest-glob=*.txt\n",
        "mod.py": "def add(a, b):\n    '''\n    >>> add(1, 2)\n    3\n    '''\n    return a + b\n\n"
        "def bad():\n    '''\n    >>> bad()\n    1\n    '''\n    return 2\n",
        "doc.txt": ">>> 1 + 1\n2\n",
        "test_plain.py": "def test_p():\n    pass\n",
    },
    "skip_xfail": {
        "test_sx.py": "import sys, pytest\n\n"
        "@pytest.mark.skip(reason='r')\ndef test_skip():\n    pass\n\n"
        "@pytest.mark.skipif(sys.version_info > (3,), reason='py3')\n"
        "def test_skipif():\n    pass\n\n"
        "@pytest.mark.xfail(reason='x')\ndef test_xfail():\n    assert 0\n\n"
        "@pytest.mark.xfail(strict=True)\ndef test_xpass_strict():\n    pass\n\n"
        "@pytest.mark.xfail\ndef test_xpass():\n    pass\n\n"
        "def test_imperative_skip():\n    pytest.skip('now')\n",
        "test_modskip.py": "import pytest\n\n"
        "pytest.skip('whole module', allow_module_level=True)\n\n"
        "def test_never():\n    assert 0\n",
    },
    "unittest_subtest": {
        "test_u.py": "import unittest\n\n"
        "class T(unittest.TestCase):\n"
        "    def test_sub(self):\n"
        "        for i in range(3):\n"
        "            with self.subTest(i=i):\n"
        "                self.assertNotEqual(i, 1)\n\n"
        "def test_ok():\n    pass\n",
    },
    "subtests_fixture": {
        "test_n.py": "def test_native(subtests):\n"
        "    for i in range(3):\n"
        "        with subtests.test(i=i):\n"
        "            assert i != 2\n\n"
        "def test_ok():\n    pass\n",
    },
    # tmp_basetemp (40 tmp_path tests + --basetemp in addopts) is MG-02: its
    # failure is timing-dependent, so it runs 5 times there instead of once.
}


def gate_migration_parity(g, args, binary):
    print("== migration: pytest parity matrix (MG-01) ==")
    for name, files in _SUITES.items():
        for rel, src in files.items():
            g.write(f"mg_parity/{name}/{rel}", src)
        cwd = g.tmp / "mg_parity" / name
        p = _pytest(g, cwd, "--junitxml", str(cwd / "pj.xml"))
        # pytest writes an empty JUnit file on a usage error, rstest none:
        # both mean "no per-test outcomes".
        pc, pj = _counts(_last(p.stdout)), _junit(cwd / "pj.xml") or None
        for n in ("0", "2"):
            jx = cwd / f"r{n}.xml"
            r = g.run("-n", n, "--junitxml", str(jx), cwd=cwd)
            rc, rj = _counts(_last(r.stdout)), _junit(jx) or None
            same = r.returncode == p.returncode and rc == pc and rj == pj
            diff = {k: (pj or {}).get(k) for k in set(pj or {}) ^ set(rj or {})}
            both = set(pj or {}) & set(rj or {})
            diff.update({k: (pj[k], rj[k]) for k in both if pj[k] != rj[k]})
            check(
                f"MG-01 {name}: rstest -n {n} matches pytest (exit, counts, per-test)",
                same,
                f"pytest rc={p.returncode} {pc} | rstest rc={r.returncode} {rc} | diff={diff}",
            )
        if name in ("cfg_required", "cfg_minversion"):
            check(
                f"MG-01 {name} setup: pytest itself exits 4",
                p.returncode == 4,
                f"rc={p.returncode} " + _out(p)[-200:],
            )


# ---------------------------------------------------------------- MG-02/03


def gate_migration_tmp_subtests(g, args, binary):
    print("== migration: --basetemp and subtest accounting (MG-02, MG-03) ==")

    # MG-02: --basetemp in addopts is the common CI setup. xdist gives each
    # worker bt/gwN; a shared root lets one worker's startup rm_rf delete
    # another's live tmp_path.
    g.write("mg_basetemp/pytest.ini", "[pytest]\naddopts = --basetemp=bt\n")
    g.write(
        "mg_basetemp/test_tmp.py",
        "import time, pytest\n\n"
        "@pytest.mark.parametrize('i', range(40))\n"
        "def test_tmp(tmp_path, i):\n"
        "    (tmp_path / 'f.txt').write_text(str(i))\n"
        "    time.sleep(0.01)\n"
        "    assert (tmp_path / 'f.txt').read_text() == str(i)\n",
    )
    cwd = g.tmp / "mg_basetemp"
    r = g.run("-n", "0", cwd=cwd)
    check(
        "MG-02 setup: --basetemp suite is green at -n 0",
        r.returncode == 0 and "40 passed" in r.stdout,
        f"rc={r.returncode} " + _last(r.stdout),
    )
    results, dirs_ok = [], True
    for _ in range(5):
        r = g.run("-n", "4", cwd=cwd)
        results.append((r.returncode, _last(r.stdout)))
        dirs_ok = dirs_ok and all((cwd / "bt" / f"gw{i}").is_dir() for i in range(4))
    check(
        "MG-02 --basetemp: 40 passed, exit 0 in all 5 runs at -n 4",
        all(rc == 0 and "40 passed" in last for rc, last in results),
        str(results),
    )
    check(
        "MG-02 --basetemp: per-worker bt/gw0..gw3 directories (xdist layout)",
        dirs_ok,
        str(sorted(p.name for p in (cwd / "bt").iterdir()) if (cwd / "bt").is_dir() else "no bt"),
    )

    # MG-03: a failing unittest subTest must be a failure in every artifact,
    # not just the process exit code.
    g.write("mg_subtest/test_u.py", _SUITES["unittest_subtest"]["test_u.py"])
    cwd = g.tmp / "mg_subtest"
    p = _pytest(g, cwd)
    oracle = _last(p.stdout)
    check(
        "MG-03 setup: pytest reports the subtest failure (exit 1, '1 failed')",
        p.returncode == 1 and "1 failed" in oracle,
        oracle,
    )
    rj, jx = cwd / "r.json", cwd / "j.xml"
    r = g.run("-n", "2", "--report-json", str(rj), "--junitxml", str(jx), cwd=cwd)
    last = _last(r.stdout)
    check("MG-03 -n 2: exit 1", r.returncode == 1, f"rc={r.returncode}")
    check(
        "MG-03 -n 2: summary counts match pytest's (incl. '1 failed')",
        "1 failed" in last and _counts(last) == _counts(oracle),
        f"rstest: {last} | pytest: {oracle}",
    )
    try:
        failed = json.loads(rj.read_text(encoding="utf-8"))["meta"]["counts"]["failed"]
    except (OSError, ValueError, KeyError):
        failed = None
    check(
        "MG-03 -n 2: report-json meta.counts.failed >= 1",
        failed is not None and failed >= 1,
        f"failed={failed}",
    )
    junit = _junit(jx) or {}
    check(
        "MG-03 -n 2: JUnit has a <failure> for test_sub",
        "failure" in junit.get("test_u.T::test_sub", []),
        str(junit),
    )
    # The same report-json undercount at -n 0, where the terminal summary is
    # pytest's own ('1 failed'): the recorder keeps the parent's call=passed.
    rj0 = cwd / "r0.json"
    r = g.run("-n", "0", "--report-json", str(rj0), cwd=cwd)
    try:
        failed0 = json.loads(rj0.read_text(encoding="utf-8"))["meta"]["counts"]["failed"]
    except (OSError, ValueError, KeyError):
        failed0 = None
    check(
        "MG-03 setup: -n 0 terminal summary says '1 failed', exit 1",
        r.returncode == 1 and "1 failed" in _last(r.stdout),
        f"rc={r.returncode} " + _last(r.stdout),
    )
    check(
        "MG-03 -n 0: report-json meta.counts.failed >= 1",
        failed0 is not None and failed0 >= 1,
        f"failed={failed0}",
    )


# ---------------------------------------------------------------- MG-04/05/11


def gate_migration_readiness(g, args, binary):
    print("== migration: readiness tools (MG-04, MG-05, MG-11) ==")

    # MG-04: parametrize over a set -> hash-randomized order per process.
    # xdist rejects it ("Different tests were collected"); the pool rejects
    # it too, so migrate-check must not call it ready.
    g.write(
        "mg_hashorder/test_set.py",
        "import pytest\n\n"
        "NAMES = {'alpha', 'beta', 'gamma', 'delta', 'epsilon', 'zeta', 'eta', 'theta'}\n\n"
        "@pytest.mark.parametrize('n', NAMES)\n"
        "def test_n(n):\n    pass\n",
    )
    cwd = g.tmp / "mg_hashorder"
    orders = set()
    for seed in ("1", "2", "3"):
        p = _pytest(g, cwd, "--collect-only", "-q", env_extra={"PYTHONHASHSEED": seed})
        orders.add(tuple(ln for ln in p.stdout.splitlines() if "::" in ln))
    check(
        "MG-04 setup: collection order differs across PYTHONHASHSEED values",
        len(orders) > 1 and all(len(o) == 8 for o in orders),
        f"distinct orders={len(orders)}",
    )
    r = g.run("-n", "4", cwd=cwd, env_drop=("PYTHONHASHSEED",))
    check(
        "MG-04 setup: the pool rejects the suite (workers collected different sets)",
        r.returncode != 0 and "different" in _out(r),
        f"rc={r.returncode} " + _out(r)[-300:],
    )
    tries = []
    for _ in range(3):
        r = g.run("migrate-check", cwd=cwd, env_drop=("PYTHONHASHSEED",))
        flagged = r.returncode != 0 and "UNSTABLE NODEIDS: none" not in r.stdout
        tries.append((r.returncode, flagged))
    check(
        "MG-04 migrate-check: order-unstable collection flagged, exit != 0 (3 tries)",
        all(f for _, f in tries),
        str(tries),
    )

    # MG-05: one file, four tests contending on one fixed path (a
    # create-exclusive lock). Any real parallel pass fails them.
    g.write(
        "mg_parallel_only/test_lock.py",
        "import os, time, pytest\n\n"
        "LOCK = os.path.join(os.path.dirname(__file__), 'shared.lock')\n\n"
        "@pytest.mark.parametrize('i', range(4))\n"
        "def test_lock(i):\n"
        "    fd = os.open(LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY)\n"
        "    try:\n"
        "        time.sleep(0.3)\n"
        "    finally:\n"
        "        os.close(fd)\n"
        "        os.unlink(LOCK)\n",
    )
    cwd = g.tmp / "mg_parallel_only"
    r0 = g.run("-n", "0", cwd=cwd)
    r4 = g.run("-n", "4", cwd=cwd)
    check(
        "MG-05 setup: green at -n 0, red at -n 4 (a real parallel-only failure)",
        r0.returncode == 0 and r4.returncode == 1 and "failed" in _last(r4.stdout),
        f"-n0 rc={r0.returncode} -n4 rc={r4.returncode} " + _last(r4.stdout),
    )
    r = g.run("migrate-check", cwd=cwd)
    check(
        "MG-05 migrate-check: reports the parallel-only failure, exit != 0",
        r.returncode != 0 and "PARALLEL: ready" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-300:],
    )

    # MG-11: xdist-removal-check, one finding per site, with its own fix.
    # (`--xdist-trial` with xdist installed is skipped: the gate venv has no
    # pytest-xdist and must not get one.)
    g.write(
        "mg_xrc/conftest.py",
        "from xdist import is_xdist_worker\nimport xdist.plugin as xp\n\n"
        "def pytest_configure(config):\n    pass\n",
    )
    g.write("mg_xrc/test_a.py", "def test_a():\n    pass\n")
    cwd = g.tmp / "mg_xrc"
    jf = cwd / "f.json"
    r = g.run("xdist-removal-check", "--xdist-removal-json", str(jf), cwd=cwd)
    doc = json.loads(jf.read_text(encoding="utf-8")) if jf.is_file() else {"findings": []}
    imports = {f["location"]: f for f in doc["findings"] if f.get("kind") == "import"}
    worker = imports.get("conftest.py:1", {})
    plugin = imports.get("conftest.py:2", {})
    check(
        "MG-11 imports: exit != 0, one finding per import line",
        r.returncode != 0
        and len(imports) == 2
        and len([f for f in doc["findings"] if f.get("kind") == "import"]) == 2,
        f"rc={r.returncode} " + str(list(imports)),
    )
    check(
        "MG-11 'from xdist import is_xdist_worker': fix names is_xdist_worker",
        "is_xdist_worker" in worker.get("fix", ""),
        worker.get("fix", ""),
    )
    check(
        "MG-11 'import xdist.plugin': fix is not the is_xdist_worker advice",
        bool(plugin.get("fix")) and "is_xdist_worker" not in plugin.get("fix", ""),
        plugin.get("fix", ""),
    )
    variants = {
        "nauto": ("-nauto", "-n"),
        "neq4": ("-n=4", "-n"),
        "numprocesses": ("--numprocesses 4", "--numprocesses"),
        "dist": ("--dist loadscope", "--dist"),
        "pxdist": ("-p xdist", "-p xdist"),
    }
    for key, (addopts, needle) in variants.items():
        g.write(f"mg_xrc_{key}/pytest.ini", f"[pytest]\naddopts = {addopts}\n")
        g.write(f"mg_xrc_{key}/test_a.py", "def test_a():\n    pass\n")
        cwd = g.tmp / f"mg_xrc_{key}"
        jf = cwd / "f.json"
        r = g.run("xdist-removal-check", "--xdist-removal-json", str(jf), cwd=cwd)
        doc = json.loads(jf.read_text(encoding="utf-8")) if jf.is_file() else {"findings": []}
        hits = [f for f in doc["findings"] if "addopts" in f.get("location", "")]
        check(
            f"MG-11 addopts '{addopts}': listed once with a fix, exit != 0",
            r.returncode != 0
            and len(hits) == 1
            and needle in hits[0].get("text", "")
            and bool(hits[0].get("fix")),
            f"rc={r.returncode} " + str(hits)[:300],
        )


# ---------------------------------------------------------------- MG-06/07/09


# Long options rstest owns that a popular plugin (or pytest core) also
# defines. Hand-maintained: the docs table must list each of these.
_PLUGIN_FLAGS = {
    "--output": "pytest-playwright",
    "--timeout": "pytest-timeout",
    "--reruns": "pytest-rerunfailures",
    "--only-rerun": "pytest-rerunfailures",
    "--html": "pytest-html",
    "--junitxml": "pytest core",
    "--debug": "pytest core",
    "--dist": "pytest-xdist",
    "--numprocesses": "pytest-xdist",
}


def gate_migration_flags(g, args, binary):
    print("== migration: plugin flags and worker identity (MG-06, MG-07, MG-09) ==")

    # MG-06: a conftest option named like an rstest option. Either it
    # reaches the plugin, or rstest refuses loudly with the `--` escape.
    g.write(
        "mg_output/conftest.py",
        "def pytest_addoption(parser):\n    parser.addoption('--output', default='test-results')\n",
    )
    g.write(
        "mg_output/test_o.py",
        "def test_o(request):\n    assert request.config.getoption('--output') == 'artifacts'\n",
    )
    cwd = g.tmp / "mg_output"
    p = _pytest(g, cwd, "--output", "artifacts")
    check("MG-06 setup: pytest passes with --output artifacts", p.returncode == 0, _last(p.stdout))
    for n in ("0", "2"):
        r = g.run("-n", n, "--output", "artifacts", cwd=cwd)
        out = _out(r)
        ok = (r.returncode == 0 and "1 passed" in r.stdout) or (
            r.returncode == 4 and "-- --output artifacts" in out
        )
        check(
            f"MG-06 -n {n} --output artifacts: reaches the plugin or exits 4 with the '--' hint",
            ok,
            f"rc={r.returncode} " + out[-300:],
        )
    r = g.run("-n", "2", "--", "--output", "artifacts", cwd=cwd)
    check(
        "MG-06 '-- --output artifacts' workaround reaches the plugin",
        r.returncode == 0 and "1 passed" in r.stdout,
        f"rc={r.returncode} " + _out(r)[-300:],
    )

    # MG-07: docs-lint. Every rstest long option that a popular plugin also
    # defines is listed in the shadowed-flags table.
    r = g.run("--help")
    owned = set(re.findall(r"^\s+(?:-\w, )?(--[a-z][a-z0-9-]*)", r.stdout, re.M))
    table = (REPO / "docs" / "_snippets" / "shadowed-flags.md").read_text(encoding="utf-8")
    # First column of each table row only: the prose cells mention flags too.
    first = [ln.split("|")[1] for ln in table.splitlines() if ln.startswith("|")]
    listed = {f for cell in first for f in re.findall(r"`(--[a-z][a-z0-9-]*)", cell)}
    check(
        "MG-07 setup: --help lists the owned flags this lint relies on",
        {"--output", "--timeout", "--reruns", "--html"} <= owned,
        str(sorted(owned))[:300],
    )
    for flag, plugin in _PLUGIN_FLAGS.items():
        if flag not in owned:
            continue
        check(
            f"MG-07 shadowed-flags.md lists {flag} ({plugin})",
            flag in listed,
            f"listed={sorted(listed)}",
        )

    # MG-09: testrun_uid in xdist's format (uuid4().hex) and identical on
    # every worker. (Plain resolution is covered by worker_identity_fixtures.)
    g.write("mg_uid/conftest.py", _LOG)
    for i in range(4):
        g.write(
            f"mg_uid/test_u{i}.py",
            "import os, uuid\nfrom conftest import _log\n\n"
            "def test_shape(testrun_uid):\n"
            "    _log(uid=testrun_uid, env=os.environ.get('PYTEST_XDIST_TESTRUNUID'))\n\n"
            "def test_uuid(testrun_uid):\n"
            "    uuid.UUID(testrun_uid)\n"
            "    assert len(os.environ['PYTEST_XDIST_TESTRUNUID']) == 32\n",
        )
    cwd = g.tmp / "mg_uid"
    log = cwd / "uid.log"
    r = g.run("-n", "2", cwd=cwd, env_extra={"MG_LOG": str(log)})
    rows = _logs(log)
    uids = {(x["uid"], x["env"]) for x in rows}
    check(
        "MG-09 testrun_uid identical on all workers (and equals the env var)",
        len(rows) == 4
        and len({x["w"] for x in rows}) == 2
        and len(uids) == 1
        and all(u == e for u, e in uids),
        str(uids) + f" workers={sorted({x['w'] for x in rows})}",
    )
    check(
        "MG-09 testrun_uid is a 32-hex uuid like xdist's",
        r.returncode == 0 and "8 passed" in r.stdout,
        _last(r.stdout) + " " + str(uids),
    )


# ---------------------------------------------------------------- MG-08/12/13


def gate_migration_scheduling(g, args, binary):
    print("== migration: groups, serial session, dispatch order (MG-08, MG-12, MG-13) ==")

    # MG-08: two xdist_groups spread over four files, plus free tests. At
    # -n 4 the groups must land on different workers; xdist runs this in
    # ~0.4s of test time.
    g.write("mg_groups/conftest.py", _LOG)
    for i in range(4):
        g.write(
            f"mg_groups/test_g{i}.py",
            "import time, pytest\nfrom conftest import _log\n\n"
            "def _run(name):\n"
            "    start = time.time()\n"
            "    time.sleep(0.1)\n"
            "    _log(name=name, start=start, end=time.time())\n\n"
            "@pytest.mark.xdist_group('db')\ndef test_db():\n    _run('db')\n\n"
            "@pytest.mark.xdist_group('net')\ndef test_net():\n    _run('net')\n\n"
            "def test_free():\n    _run('free')\n",
        )
    cwd = g.tmp / "mg_groups"
    log = cwd / "grp.log"
    runs = []
    for _ in range(3):
        _clear(log)
        r = g.run("-n", "4", "--dist", "loadgroup", "-v", cwd=cwd, env_extra={"MG_LOG": str(log)})
        rows = _logs(log)
        db = {x["w"] for x in rows if x["name"] == "db"}
        net = {x["w"] for x in rows if x["name"] == "net"}
        span = max(x["end"] for x in rows) - min(x["start"] for x in rows) if rows else 99
        runs.append((r.returncode, len(rows), sorted(db), sorted(net), round(span, 2)))
    check(
        "MG-08 setup: 12 passed, each group cohesive on one worker (x3)",
        all(rc == 0 and n == 12 and len(d) == 1 and len(t) == 1 for rc, n, d, t, _ in runs),
        str(runs),
    )
    check(
        "MG-08 loadgroup -n 4: db and net groups on different workers (x3)",
        all(d != t for _, _, d, t, _ in runs),
        str(runs),
    )
    check(
        "MG-08 loadgroup -n 4: test-time span < 0.7s (x3)",
        all(span < 0.7 for *_, span in runs),
        str(runs),
    )

    # MG-12: serial tests reuse the designated worker's session (no second
    # sess-up). Ordering and exclusivity are covered by gate_serial_mark.
    g.write(
        "mg_serial/conftest.py",
        _LOG + "\nimport pytest\n\n"
        "@pytest.fixture(scope='session', autouse=True)\n"
        "def sess():\n    _log(ev='sess-up')\n    yield\n    _log(ev='sess-down')\n",
    )
    g.write(
        "mg_serial/test_s.py",
        "import time, pytest\nfrom conftest import _log\n\n"
        "@pytest.mark.parametrize('i', range(6))\n"
        "def test_par(i):\n    time.sleep(0.05)\n    _log(ev='par')\n\n"
        "@pytest.mark.serial\ndef test_serial_one():\n    _log(ev='serial')\n\n"
        "@pytest.mark.serial\ndef test_serial_two():\n    _log(ev='serial')\n",
    )
    cwd = g.tmp / "mg_serial"
    log = cwd / "ser.log"
    r = g.run("-n", "3", cwd=cwd, env_extra={"MG_LOG": str(log)})
    rows = sorted(_logs(log), key=lambda x: x["t"])
    ser = [x for x in rows if x["ev"] == "serial"]
    check(
        "MG-12 setup: 8 passed, both serial tests on one worker",
        r.returncode == 0
        and "8 passed" in r.stdout
        and len(ser) == 2
        and len({x["w"] for x in ser}) == 1,
        _last(r.stdout) + " " + str(ser),
    )
    ups = [x for x in rows if x["ev"] == "sess-up"]
    between = [x for x in ups if ser and ser[0]["t"] <= x["t"] <= ser[-1]["t"]]
    ups_on_ser_worker = [x for x in rows if ser and x["ev"] == "sess-up" and x["w"] == ser[0]["w"]]
    check(
        "MG-12 serial tests reuse the designated worker's session (one sess-up there)",
        not between and len(ups_on_ser_worker) == 1,
        f"sess-up between serials={len(between)} on serial worker={len(ups_on_ser_worker)}",
    )

    # MG-13: docs agree on what a reordering pytest_collection_modifyitems
    # does at -n >= 2, and the runtime matches them.
    docs = {
        "migrate-from-pytest.md": REPO / "docs" / "guides" / "migrate-from-pytest.md",
        "xdist-support.md": REPO / "docs" / "reference" / "xdist-support.md",
    }
    claims = {}
    for label, path in docs.items():
        text = " ".join(path.read_text(encoding="utf-8").split())
        claims[label] = bool(
            re.search(r"[Rr]eordering[^.]{0,80}modifyitems[^.]{0,40} is ignored", text)
            or re.search(r"modifyitems[`*]* reordering is ignored", text)
        )
    check(
        "MG-13 docs: migrate-from-pytest.md and xdist-support.md agree on reordering",
        len(set(claims.values())) == 1,
        str(claims),
    )
    g.write(
        "mg_reorder/conftest.py",
        _LOG + "\ndef pytest_collection_modifyitems(items):\n    items.reverse()\n",
    )
    g.write(
        "mg_reorder/test_r.py",
        "import time, pytest\nfrom conftest import _log\n\n"
        "@pytest.mark.parametrize('i', range(20))\n"
        "def test_r(i):\n    time.sleep(0.02)\n    _log(i=i)\n",
    )
    cwd = g.tmp / "mg_reorder"
    log = cwd / "ord.log"
    r = g.run("-n", "2", cwd=cwd, env_extra={"MG_LOG": str(log)})
    rows = sorted(_logs(log), key=lambda x: x["t"])
    per_worker = {}
    for x in rows:
        per_worker.setdefault(x["w"], []).append(x["i"])
    follows = (
        r.returncode == 0
        and len(rows) == 20
        and all(seq == sorted(seq, reverse=True) for seq in per_worker.values())
        and 19 in [x["i"] for x in rows[:2]]
    )
    check(
        "MG-13 cold-cache -n 2 dispatch follows the hook's reversed order",
        follows,
        str(per_worker),
    )


# ---------------------------------------------------------------- MG-10


def gate_migration_lazy(g, args, binary):
    print("== migration: lazy collection parity (MG-10) ==")
    cases = {
        # (a) overriding norecursedirs drops pytest's default '.*', so a
        # hidden directory is collected.
        "hidden": {
            "pytest.ini": "[pytest]\nnorecursedirs = legacy\n",
            "tests/test_a.py": "def test_a():\n    pass\n",
            "tests/.hidden/test_h.py": "def test_h():\n    pass\n",
            "legacy/test_old.py": "def test_old():\n    assert 0\n",
        },
        # (b) a ripgrep-style .ignore file means nothing to pytest.
        "dotignore": {
            ".ignore": "b/\n",
            "a/test_a.py": "def test_a():\n    pass\n",
            "b/test_b.py": "def test_b():\n    pass\n",
        },
        # (c) glob testpaths.
        "globpaths": {
            "pytest.ini": "[pytest]\ntestpaths = pkgs/*/tests\n",
            "pkgs/one/tests/test_1.py": "def test_1():\n    pass\n",
            "pkgs/two/tests/test_2.py": "def test_2():\n    pass\n",
            "other/test_x.py": "def test_x():\n    assert 0\n",
        },
    }
    for name, files in cases.items():
        for rel, src in files.items():
            g.write(f"mg_lazy_{name}/{rel}", src)
        cwd = g.tmp / f"mg_lazy_{name}"
        p = _pytest(g, cwd)
        r = g.run("-n", "2", "--collect", "lazy", cwd=cwd)
        pc, rc = _counts(_last(p.stdout)), _counts(_last(r.stdout))
        check(
            f"MG-10 {name} setup: pytest collects and passes the expected tests",
            p.returncode == 0 and pc.get("passed", 0) == 2,
            _last(p.stdout),
        )
        check(
            f"MG-10 {name}: --collect lazy -n 2 matches pytest",
            r.returncode == p.returncode and rc == pc,
            f"pytest {pc} | rstest rc={r.returncode} {rc} " + _out(r)[-200:],
        )
