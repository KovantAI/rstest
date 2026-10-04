"""e2e gate sections: CI / platform engineer (CI-* scenarios).

A platform engineer wires rstest into a pipeline: copies a recipe from the
docs, adds JUnit / report-json / annotation output, shards across jobs, and
gates the build on the exit code and the artifacts. These sections check that
the exit code and every artifact agree, that documented snippets fail the job
when a test fails, and that tool-side problems never flip a test result.
Checks tagged `known_bug=True` pin current failures: they xfail today and turn
the gate red once the bug is fixed, so the marker gets dropped and the check
becomes a regression guard.
"""

import contextlib
import json
import os
import re
import shutil
import signal
import subprocess
import textwrap
import time
import xml.etree.ElementTree as ET

from _harness import REPO, WINDOWS, check, find_python, git_init_commit, venv_bin

DOCS = REPO / "docs"
ACTION = REPO / ".github" / "actions" / "rstest" / "action.yml"
CURSOR_RE = re.compile(r"\x1b\[\d*[ABCDJK]|\x1b\[\?25[lh]")
SGR_RE = re.compile(r"\x1b\[[\d;]*m")


def _out(r):
    return r.stdout + r.stderr


def _load(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _exitstatus(path):
    doc = _load(path)
    try:
        return doc["meta"]["exitstatus"]
    except (TypeError, KeyError):
        return None


def _base_env(g, extra=None, drop=()):
    """The environment Gate.run builds, for drivers that need their own
    process handling (pty, signals, shell snippets)."""
    env = dict(os.environ, VIRTUAL_ENV=str(g.venv), RSTEST_WORKER_PATH=str(REPO / "python"))
    for k in (
        "PYTEST_ADDOPTS",
        "GITHUB_STEP_SUMMARY",
        "BUILDKITE",
        "GITHUB_BASE_REF",
        "CI_MERGE_REQUEST_DIFF_BASE_SHA",
        "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
        "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        "CI",
        "GITHUB_ACTIONS",
        "NO_COLOR",
        "FORCE_COLOR",
        "PY_COLORS",
    ):
        env.pop(k, None)
    env.update(extra or {})
    for k in drop:
        env.pop(k, None)
    return env


def _write_exec(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    path.chmod(0o755)
    return path


# ---------------------------------------------------------------- doc parsing


def _doc_blocks(path):
    """Fenced code blocks in a markdown file, as dedented strings. Handles
    fences indented under list items."""
    blocks, cur, indent = [], None, 0
    for ln in path.read_text(encoding="utf-8").splitlines():
        s = ln.lstrip()
        if cur is None:
            if s.startswith("```"):
                cur, indent = [], len(ln) - len(s)
        elif s.startswith("```"):
            blocks.append("\n".join(x[indent:] if x[:indent].isspace() else x for x in cur))
            cur = None
        else:
            cur.append(ln)
    return blocks


def _doc_block(path, *needles):
    """The first fenced block in `path` containing every needle, or None."""
    for b in _doc_blocks(path):
        if all(n in b for n in needles):
            return b
    return None


def _yaml_literal(text, key_re):
    """Body of the first `key: |` literal scalar whose key line matches
    `key_re`: the following lines indented deeper than the key, dedented."""
    lines = text.splitlines()
    for i, ln in enumerate(lines):
        if re.match(key_re, ln) and ln.rstrip().endswith("|"):
            # Column of the key itself, past any `- ` sequence marker.
            ind = len(ln) - len(ln.lstrip(" -"))
            body = []
            for x in lines[i + 1 :]:
                if x.strip() and len(x) - len(x.lstrip()) <= ind:
                    break
                body.append(x)
            return textwrap.dedent("\n".join(body)).strip("\n") + "\n"
    return None


def _yaml_list(text, key):
    """Items of the first `key:` block sequence (`- item` lines)."""
    lines = text.splitlines()
    for i, ln in enumerate(lines):
        if ln.strip() == f"{key}:":
            ind = len(ln) - len(ln.lstrip())
            items = []
            for x in lines[i + 1 :]:
                if x.strip() and len(x) - len(x.lstrip()) <= ind:
                    break
                if x.strip().startswith("- "):
                    items.append(x.strip()[2:])
            return items
    return None


# ------------------------------------------------------------- report checks


def _schema_errors(doc, schema, node=None, path="$"):
    """Minimal draft-07 subset validator (type, required, properties,
    additionalProperties, items, $ref, allOf, minimum): enough for the
    report-json schema without a jsonschema dependency."""
    node = schema if node is None else node
    if "$ref" in node:
        ref = node["$ref"].split("/")[-1]
        return _schema_errors(doc, schema, schema["definitions"][ref], path)
    errs = []
    for sub in node.get("allOf", []):
        errs += _schema_errors(doc, schema, sub, path)
    types = node.get("type")
    if types:
        tmap = {
            "object": dict,
            "array": list,
            "string": str,
            "boolean": bool,
            "integer": int,
            "number": (int, float),
            "null": type(None),
        }
        tl = types if isinstance(types, list) else [types]
        ok = any(
            isinstance(doc, tmap[t]) and not (t in ("integer", "number") and isinstance(doc, bool))
            for t in tl
        )
        if not ok:
            return [*errs, f"{path}: expected {types}, got {type(doc).__name__}"]
    if "minimum" in node and isinstance(doc, (int, float)) and doc < node["minimum"]:
        errs.append(f"{path}: {doc} < {node['minimum']}")
    if isinstance(doc, dict):
        for k in node.get("required", []):
            if k not in doc:
                errs.append(f"{path}: missing {k}")
        props = node.get("properties", {})
        for k, v in doc.items():
            if k in props:
                errs += _schema_errors(v, schema, props[k], f"{path}.{k}")
            elif isinstance(node.get("additionalProperties"), dict):
                errs += _schema_errors(v, schema, node["additionalProperties"], f"{path}.{k}")
    if isinstance(doc, list) and isinstance(node.get("items"), dict):
        for i, v in enumerate(doc):
            errs += _schema_errors(v, schema, node["items"], f"{path}[{i}]")
    return errs


def _junit_summary(path):
    """(attribute counts, child-element counts, collect-error testcases) for
    the single <testsuite> in a JUnit file, or None if unreadable."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError):
        return None
    ts = root if root.tag == "testsuite" else root.find("testsuite")
    if ts is None:
        return None
    cases = list(ts.iter("testcase"))
    attrs = {k: int(ts.get(k, "0")) for k in ("tests", "failures", "errors", "skipped")}
    kids = {
        "tests": len(cases),
        "failures": sum(len(c.findall("failure")) for c in cases),
        "errors": sum(len(c.findall("error")) for c in cases),
        "skipped": sum(len(c.findall("skipped")) for c in cases),
    }
    collect = sum(
        1 for c in cases for e in c.findall("error") if e.get("message") == "collection failure"
    )
    return attrs, kids, collect


# ------------------------------------------------------------------ sections


def gate_ci_exit_codes(g, args, binary):
    print("== ci: exit code table and shard validation (CI-01, CI-07) ==")
    py = venv_bin(g.venv, "python")
    suites = {
        "pass": {"test_p.py": "def test_a():\n    pass\n\ndef test_b():\n    pass\n"},
        "fail": {"test_f.py": "def test_a():\n    pass\n\ndef test_b():\n    assert 0\n"},
        "coll": {
            "test_ok.py": "def test_a():\n    pass\n",
            "test_c.py": "import ci_nonexistent_mod\n",
        },
        "skip": {
            "test_s.py": "import pytest\n\n"
            "@pytest.mark.skip\ndef test_a():\n    pass\n\n"
            "@pytest.mark.skip\ndef test_b():\n    pass\n"
        },
        "xpass": {
            "test_x.py": "import pytest\n\n"
            "@pytest.mark.xfail(strict=True)\ndef test_a():\n    pass\n\n"
            "def test_b():\n    pass\n"
        },
        "empty": {".keep": ""},
    }
    for name, files in suites.items():
        for fn, src in files.items():
            g.write(f"ci_exit/{name}/{fn}", src)
    cwd = g.tmp / "ci_exit"
    (cwd / ".git").mkdir(exist_ok=True)  # bound rootdir discovery
    # (situation, args, expected exit) per docs/reference/exit-codes.md.
    table = [
        ("all pass", ["pass"], 0),
        ("one failure", ["fail"], 1),
        ("collection error", ["coll"], 2),
        ("bad flag", ["pass", "--ci-bogus-flag"], 4),
        ("bad -m expression", ["pass", "-m", "a and"], 4),
        ("missing path", ["pass/missing.py"], 4),
        ("no tests", ["empty"], 5),
        ("-k matches nothing", ["pass", "-k", "ci_nomatch"], 5),
        ("all skipped", ["skip"], 0),
        ("strict xpass", ["xpass"], 1),
    ]
    oracle = {
        sit: subprocess.run(
            [str(py), "-m", "pytest", "-q", "-p", "no:cacheprovider", *a],
            cwd=cwd,
            capture_output=True,
            timeout=60,
        ).returncode
        for sit, a, _ in table
    }
    bad = {s: (oracle[s], exp) for s, _, exp in table if oracle[s] != exp}
    check("CI-01 setup: pytest agrees with the documented exit table", not bad, str(bad))
    rj = g.tmp / "ci_exit.json"
    for mode in ("0", "2"):
        for sit, a, exp in table:
            rj.unlink(missing_ok=True)
            r = g.run("-n", mode, *a, "--report-json", str(rj), cwd=cwd)
            es = _exitstatus(rj)
            check(
                f"CI-01 -n {mode} {sit}: exit {exp} and report-json meta.exitstatus agrees",
                r.returncode == exp and es == exp,
                f"rc={r.returncode} exitstatus={es} " + _out(r)[-200:],
            )

    # SIGINT, as a CI runner's cancel (or Ctrl-C) delivers it to the process
    # group. pytest exits 2 with a KeyboardInterrupt summary.
    if WINDOWS:
        print("  skip  CI-01 SIGINT checks (POSIX signals)")
    else:
        g.write(
            "ci_exit/sigint/test_slow.py",
            "import time\n\ndef test_quick():\n    pass\n\n"
            "def test_slow_a():\n    time.sleep(30)\n\n"
            "def test_slow_b():\n    time.sleep(30)\n",
        )
        for mode, bug in (("0", True), ("2", False)):
            rj.unlink(missing_ok=True)
            p = subprocess.Popen(
                [str(binary), "-n", mode, "sigint", "--report-json", str(rj)],
                cwd=cwd,
                env=_base_env(g),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            try:
                time.sleep(2.0)
                os.killpg(p.pid, signal.SIGINT)
                out, err = p.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                out, err = "", "timed out after SIGINT"
            finally:
                with contextlib.suppress(OSError):
                    os.killpg(p.pid, signal.SIGKILL)
                p.wait()
            es = _exitstatus(rj)
            check(
                f"CI-01 -n {mode} SIGINT: exit 2 and report-json meta.exitstatus agrees",
                p.returncode == 2 and es == 2,
                f"rc={p.returncode} exitstatus={es} " + (out + err)[-200:].replace("\n", "|"),
                known_bug=bug,
            )

    # CI-07: a matrix variable typo must fail loudly, not run the whole suite.
    for spec in ("0/4", "5/4", "1/0", "a/b", "2", "-1/4"):
        r = g.run("-n", "2", f"--shard={spec}", "pass", cwd=cwd)
        check(
            f"CI-07 --shard {spec}: rejected with an error naming --shard",
            r.returncode not in (0, 5) and "--shard" in r.stderr and " passed" not in r.stdout,
            f"rc={r.returncode} " + r.stderr[-200:],
        )
    # More shards than tests: the surplus shards run nothing, exit 0 or 5,
    # and still write a stamped report so shard-verify can reconcile them.
    reports = []
    rcs = []
    for k in range(1, 5):
        # Same (cold) cache on every job, or a sibling's timings reshuffle
        # the partition between shards.
        shutil.rmtree(cwd / ".rstest_cache", ignore_errors=True)
        rp = g.tmp / f"ci_shard_small.{k}.json"
        rp.unlink(missing_ok=True)
        r = g.run("-n", "2", "--shard", f"{k}/4", "pass", "--report-json", str(rp), cwd=cwd)
        rcs.append(r.returncode)
        reports.append(rp)
    stamped = [(_load(p) or {}).get("meta", {}).get("shard", {}).get("k") for p in reports]
    check(
        "CI-07 --shard k/4 with 2 tests: every shard exits 0 or 5",
        all(rc in (0, 5) for rc in rcs),
        f"rcs={rcs}",
    )
    check(
        "CI-07 --shard k/4 with 2 tests: empty shards still write a stamped report",
        stamped == [1, 2, 3, 4],
        f"stamped={stamped}",
    )
    r = g.run("shard-verify", *map(str, reports), cwd=cwd)
    check(
        "CI-07 --shard k/4 with 2 tests: shard-verify accepts the empty shards",
        r.returncode == 0,
        f"rc={r.returncode} " + _out(r)[-200:],
    )


def gate_ci_artifacts(g, args, binary):
    print("== ci: artifacts and annotations agree with the exit code (CI-02, CI-04, CI-05) ==")
    suites = {
        "green": "def test_a():\n    pass\n\ndef test_b():\n    pass\n",
        "failing": "def test_ok():\n    pass\n\ndef test_bad():\n    assert 1 == 2\n",
        "fixture_errors": "import pytest\n\n"
        "@pytest.fixture\ndef broken():\n    raise RuntimeError('setup boom')\n\n"
        "@pytest.fixture\ndef bad_teardown():\n"
        "    yield\n    raise RuntimeError('teardown boom')\n\n"
        "def test_setup(broken):\n    pass\n\n"
        "def test_teardown(bad_teardown):\n    pass\n\n"
        "def test_ok():\n    pass\n",
        "skip_xfail": "import sys, pytest\n\n"
        "@pytest.mark.skip(reason='no')\ndef test_skip():\n    pass\n\n"
        "@pytest.mark.skipif(sys.platform != 'nope', reason='cond')\n"
        "def test_skipif():\n    pass\n\n"
        "@pytest.mark.xfail\ndef test_xfail():\n    assert 0\n\n"
        "@pytest.mark.xfail\ndef test_xpass():\n    pass\n\n"
        "def test_ok():\n    pass\n",
        "strict_xpass": "import pytest\n\n"
        "@pytest.mark.xfail(strict=True)\ndef test_xpass():\n    pass\n\n"
        "def test_ok():\n    pass\n",
        "ids_weird": "import pytest\n\n"
        "@pytest.mark.parametrize('v', ['a::b', 'x[1]', 'p/q', 'sp ace', 'ünï', ']]>'])\n"
        "def test_ids(v):\n    assert v != 'p/q'\n",
        "collect_error": None,
        "unittest_subtest": "import unittest\n\n"
        "class T(unittest.TestCase):\n"
        "    def test_sub(self):\n"
        "        for i in range(3):\n"
        "            with self.subTest(i=i):\n"
        "                self.assertNotEqual(i, 1)\n\n"
        "def test_ok():\n    pass\n",
    }
    schema = json.loads((DOCS / "reference/schemas/report-json.schema.json").read_text("utf-8"))
    bogus = {"collect_errors": [1], "meta": {"counts": {"x": -1}}, "tests": {"t": {"call": 0}}}
    check(
        "CI-02 setup: the hand-rolled schema check rejects a malformed report",
        len(_schema_errors(bogus, schema)) >= 4,
        str(_schema_errors(bogus, schema)),
    )
    for name, src in suites.items():
        cwd = g.tmp / f"ci_art_{name}"
        (cwd / ".git").mkdir(parents=True, exist_ok=True)
        if src is None:
            g.write(f"ci_art_{name}/tests/test_ok.py", "def test_a():\n    pass\n")
            g.write(f"ci_art_{name}/tests/test_c.py", "import ci_nonexistent_mod\n")
        else:
            # Two files so -n 2 really spreads work over a pool.
            g.write(f"ci_art_{name}/tests/test_one.py", src)
            g.write(f"ci_art_{name}/tests/test_two.py", src)
        j, rj, html = cwd / "j.xml", cwd / "r.json", cwd / "r.html"
        r = g.run(
            "-n", "2", "--junitxml", str(j), "--report-json", str(rj), "--html", str(html), cwd=cwd
        )
        doc, ju = _load(rj), _junit_summary(j)
        if doc is None or ju is None:
            check(f"CI-02 {name}: junit, report-json and html written", False, _out(r)[-300:])
            continue
        attrs, kids, collect = ju
        counts = doc["meta"]["counts"]
        rc_bad = r.returncode != 0
        junit_bad = kids["failures"] + kids["errors"] > 0
        json_bad = counts["failed"] + counts["errors"] + counts["collect_errors"] > 0
        check(
            f"CI-02 {name}: exit != 0 iff JUnit has failure/error",
            rc_bad == junit_bad,
            f"rc={r.returncode} junit={kids}",
        )
        check(
            f"CI-02 {name}: exit != 0 iff report-json counts a failure",
            rc_bad == json_bad,
            f"rc={r.returncode} counts={counts}",
        )
        check(
            f"CI-02 {name}: report-json meta.exitstatus equals the exit code",
            doc["meta"]["exitstatus"] == r.returncode,
            f"rc={r.returncode} exitstatus={doc['meta']['exitstatus']}",
        )
        check(
            f"CI-02 {name}: JUnit <testsuite> counts equal its child elements",
            attrs == kids,
            f"attrs={attrs} children={kids}",
        )
        errs = _schema_errors(doc, schema)
        check(f"CI-02 {name}: report-json matches docs/reference/schemas", not errs, str(errs[:3]))
        check(f"CI-02 {name}: html report written", html.is_file() and html.stat().st_size > 0)
        if name == "collect_error":
            check(
                "CI-02 collect error: report-json counts it once, as JUnit does",
                counts["collect_errors"] == collect == 1 and len(doc["collect_errors"]) == 1,
                f"json={counts['collect_errors']} {doc['collect_errors']} junit={collect}",
            )

    # CI-04: one module that cannot import. Every annotation style must make
    # the collect error visible to the CI system, not only to a human.
    cwd = g.tmp / "ci_art_collect_error"
    r = g.run("-n", "2", "--output", "tap", cwd=cwd)
    lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
    check("CI-04 --output tap: collect error exits 2", r.returncode == 2, f"rc={r.returncode}")
    check(
        "CI-04 --output tap: collect error is not a bare 1..0",
        any(ln.startswith("not ok") or ln.startswith("Bail out!") for ln in lines),
        repr(r.stdout[-200:]),
        known_bug=True,
    )
    check(
        "CI-04 --output tap: traceback text present in the log",
        "ci_nonexistent_mod" in _out(r),
        _out(r)[-200:],
        known_bug=True,
    )
    expect = {
        "github": lambda ln: ln.startswith("::error file=tests/test_c.py"),
        "azure": lambda ln: ln.startswith("##vso[task.logissue type=error"),
        "teamcity": lambda ln: (
            ln.startswith("##teamcity[") and ("testFailed" in ln or "buildProblem" in ln)
        ),
    }
    for style, pred in expect.items():
        r = g.run("-n", "2", "--output", style, cwd=cwd)
        check(
            f"CI-04 --output {style}: collect error exits 2 with the traceback in the log",
            r.returncode == 2 and "ci_nonexistent_mod" in _out(r),
            f"rc={r.returncode} " + _out(r)[-200:],
        )
        check(
            f"CI-04 --output {style}: collect error emits a CI annotation",
            any(pred(ln) for ln in r.stdout.splitlines()),
            r.stdout[-300:],
            known_bug=True,
        )

    # CI-05: monorepo layout from ci-quickstart: repo root cw/, the job runs
    # with working-directory: proj. Annotations must point at repo paths and
    # carry the exception, not the first traceback line.
    g.write(
        "ci_cw/proj/tests/test_a.py",
        "import pytest\n\n"
        "@pytest.fixture\ndef broken():\n    raise RuntimeError('setup boom')\n\n"
        "def test_err(broken):\n    pass\n\n"
        "def test_fail():\n    x = 1\n    assert x == 2, 'x mismatch'\n\n"
        "def test_ok():\n    pass\n",
    )
    root = g.tmp / "ci_cw"
    git_init_commit(root)
    proj = root / "proj"
    env = {"GITHUB_ACTIONS": "true", "GITHUB_WORKSPACE": str(root)}
    r = g.run("-n", "2", "--output", "github", cwd=proj, env_extra=env)
    ann = [ln for ln in r.stdout.splitlines() if ln.startswith("::error ")]
    check(
        "CI-05 setup: github emits one ::error per failing test",
        r.returncode == 1 and len(ann) == 2,
        r.stdout[-300:],
    )
    check(
        "CI-05 github: annotation message carries the exception line",
        any("RuntimeError: setup boom" in a for a in ann)
        and any("AssertionError: x mismatch" in a for a in ann),
        "\n".join(ann)[-300:],
    )
    check(
        "CI-05 github: file= is repo-relative under GITHUB_WORKSPACE",
        len(ann) == 2 and all("file=proj/tests/test_a.py" in a for a in ann),
        "\n".join(a[:80] for a in ann),
        known_bug=True,
    )
    r = g.run("-n", "2", "--output", "azure", cwd=proj, env_extra={"TF_BUILD": "True"})
    iss = [ln for ln in r.stdout.splitlines() if ln.startswith("##vso[task.logissue type=error")]
    check("CI-05 setup: azure emits one logissue per failing test", len(iss) == 2, r.stdout[-300:])
    check(
        "CI-05 azure: issue text is the exception line, not the first traceback line",
        any("RuntimeError: setup boom" in ln for ln in iss)
        and any("AssertionError: x mismatch" in ln for ln in iss),
        "\n".join(iss)[-300:],
        known_bug=True,
    )


def gate_ci_sharding(g, args, binary):
    print("== ci: sharding complete, disjoint, balanced (CI-06) ==")
    for f in range(8):
        g.write(
            f"ci_shard/tests/test_f{f}.py",
            "".join(f"def test_{i}():\n    pass\n\n" for i in range(50)),
        )
    g.write("ci_shard/tests/test_long.py", "import time\n\ndef test_long():\n    time.sleep(0.5)\n")
    cwd = g.tmp / "ci_shard"
    (cwd / ".git").mkdir(exist_ok=True)
    cache, snap = cwd / ".rstest_cache", g.tmp / "ci_shard_snapshot"
    r = g.run("-n", "2", "-q", cwd=cwd)
    check(
        "CI-06 setup: warm-up run records durations",
        r.returncode == 0 and (cache / "durations.json").is_file(),
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    shutil.copytree(cache, snap)

    def run_shards(tag, warm):
        sizes, files = [], []
        for k in range(1, 5):
            shutil.rmtree(cache, ignore_errors=True)
            if warm:
                shutil.copytree(snap, cache)  # every job restores one snapshot
            rp = g.tmp / f"ci_shard_{tag}.{k}.json"
            rp.unlink(missing_ok=True)
            g.run("-n", "2", "--shard", f"{k}/4", "--report-json", str(rp), "-q", cwd=cwd)
            tests = (_load(rp) or {}).get("tests", {})
            sizes.append((len(tests), any("test_long" in t for t in tests)))
            files.append(rp)
        return sizes, files

    for tag, warm, bug in (("warm", True, True), ("cold", False, False)):
        sizes, files = run_shards(tag, warm)
        r = g.run("shard-verify", *map(str, files), cwd=cwd)
        check(
            f"CI-06 {tag} cache: shard-verify passes over 4 shards",
            r.returncode == 0 and sum(n for n, _ in sizes) == 401,
            f"rc={r.returncode} sizes={sizes} " + _out(r)[-200:],
        )
        others = [n for n, has_long in sizes if not has_long]
        mean = 401 / 4
        check(
            f"CI-06 {tag} cache: shard sizes within 2x of the mean (long pole's shard aside)",
            len(others) >= 3 and all(mean / 2 <= n <= mean * 2 for n in others),
            f"sizes={sizes}",
            known_bug=bug,
        )


def gate_ci_side_effects(g, args, binary):
    print("== ci: tool-side failures and workspace hygiene (CI-08, CI-11) ==")
    g.write("ci_side/tests/test_ok.py", "def test_a():\n    pass\n\ndef test_b():\n    pass\n")
    g.write(
        "ci_side_red/tests/test_bad.py", "def test_a():\n    pass\n\ndef test_b():\n    assert 0\n"
    )
    cwd, red = g.tmp / "ci_side", g.tmp / "ci_side_red"
    for d in (cwd, red):
        (d / ".git").mkdir(exist_ok=True)

    # CI-08 (a): the summary path is unwritable (act, container jobs). The
    # doctor's job-summary publish is cosmetic and must not turn green red.
    summ = g.tmp / "ci_nonexistent_dir" / "summary.md"
    rj, dj = g.tmp / "ci_side.json", g.tmp / "ci_side_doctor.json"
    rj.unlink(missing_ok=True)
    r = g.run(
        "-n",
        "2",
        "--doctor-json",
        str(dj),
        "--report-json",
        str(rj),
        cwd=cwd,
        env_extra={"GITHUB_ACTIONS": "true", "GITHUB_STEP_SUMMARY": str(summ)},
    )
    check("CI-08 setup: the tests passed", "2 passed" in r.stdout, r.stdout[-200:])
    check(
        "CI-08 unwritable GITHUB_STEP_SUMMARY: report-json still written with exitstatus 0",
        _exitstatus(rj) == 0,
        f"exitstatus={_exitstatus(rj)}",
        known_bug=True,
    )
    check(
        "CI-08 unwritable GITHUB_STEP_SUMMARY: run still exits 0",
        r.returncode == 0,
        f"rc={r.returncode} " + r.stderr[-200:],
        known_bug=True,
    )
    check(
        "CI-08 unwritable GITHUB_STEP_SUMMARY: stderr names the summary path",
        str(summ) in r.stderr,
        r.stderr[-200:],
        known_bug=True,
    )
    # The Buildkite branch of the same publish already warns and keeps going.
    bin_empty = g.tmp / "ci_empty_bin"
    bin_empty.mkdir(exist_ok=True)
    path = os.pathsep.join([str(bin_empty), "/usr/bin", "/bin"])
    r = g.run(
        "-n",
        "2",
        "--doctor-json",
        str(dj),
        cwd=cwd,
        env_extra={"BUILDKITE": "true", "PATH": path},
    )
    check(
        "CI-08 missing buildkite-agent: warns and exits 0",
        r.returncode == 0 and "buildkite-agent" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )

    # CI-08 (b): ci-shared-cache.md: a failed --cache-push only warns and
    # keeps the run's own exit code; a failed --cache-pull aborts with exit 1
    # before any test runs. A remote whose segments/ is a plain file fails
    # both ways regardless of permissions (CI often runs as root).
    remote = g.tmp / "ci_bad_remote"
    remote.mkdir(exist_ok=True)
    (remote / "segments").write_text("not a dir\n", encoding="utf-8")
    for d, exp in ((cwd, 0), (red, 1)):
        r = g.run("-n", "2", "--cache-remote", str(remote), "--cache-push", cwd=d)
        check(
            f"CI-08 failed --cache-push: warns, exit stays {exp}",
            r.returncode == exp and "cache: push failed" in r.stderr,
            f"rc={r.returncode} " + r.stderr[-200:],
        )
    rj.unlink(missing_ok=True)
    r = g.run(
        "-n",
        "2",
        "--cache-remote",
        str(remote),
        "--cache-pull",
        "--report-json",
        str(rj),
        cwd=cwd,
    )
    check(
        "CI-08 failed --cache-pull: exit 1, no tests run, no report",
        r.returncode == 1
        and "Error: pulling shared cache" in r.stderr
        and " passed" not in r.stdout
        and not rj.exists(),
        f"rc={r.returncode} report={rj.exists()} " + r.stderr[-200:],
    )

    # CI-11: "tree must be clean" gates. pytest's own cache dir ignores
    # itself; rstest's caches must too.
    py = venv_bin(g.venv, "python")
    for tool in ("pytest", "rstest"):
        repo = g.tmp / f"ci_clean_{tool}"
        g.write(f"ci_clean_{tool}/tests/test_a.py", "def test_a():\n    pass\n")
        g.write(f"ci_clean_{tool}/tests/test_b.py", "def test_b():\n    pass\n")
        g.write(f"ci_clean_{tool}/.gitignore", "__pycache__/\n")  # every real repo has it
        git_init_commit(repo)
        env = {"CI": "true"}
        if tool == "pytest":
            subprocess.run(
                [str(py), "-m", "pytest", "-q"],
                cwd=repo,
                env=_base_env(g, env),
                capture_output=True,
                timeout=60,
            )
        else:
            r = g.run("-n", "2", "--junitxml", str(g.tmp / "ci_clean.xml"), cwd=repo, env_extra=env)
        st = subprocess.run(
            ["git", "status", "--porcelain", "-uall"],
            cwd=repo,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        if tool == "pytest":
            check(
                "CI-11 setup: pytest leaves a clean tree",
                st == "" and (repo / ".pytest_cache").is_dir(),
                st[:200],
            )
        else:
            check(
                "CI-11 CI=true rstest -n 2 leaves a clean tree",
                r.returncode == 0 and st == "",
                f"rc={r.returncode} porcelain={st[:200]!r}",
            )
    repo = g.tmp / "ci_clean_nocache"
    g.write("ci_clean_nocache/tests/test_a.py", "def test_a():\n    pass\n")
    r = g.run("-n", "2", "-p", "no:cacheprovider", cwd=repo, env_extra={"CI": "true"})
    check(
        "CI-11 -p no:cacheprovider: no .rstest_cache written",
        r.returncode == 0 and not (repo / ".rstest_cache").exists(),
        f"rc={r.returncode} exists={(repo / '.rstest_cache').exists()}",
        known_bug=True,
    )


def gate_ci_snippets(g, args, binary):
    print("== ci: documented snippets and the bundled action (CI-09, CI-10) ==")
    if WINDOWS:
        print("  skip  CI-09 / CI-10 (POSIX shells)")
        return
    bash = shutil.which("bash")
    if bash is None:
        print("  skip  CI-09 / CI-10 (no bash on PATH)")
        return

    # A suite with one failing test, plus fake external CLIs. `rstest` on
    # PATH is a wrapper around the gate binary.
    suite = g.tmp / "ci_snip"
    g.write(
        "ci_snip/tests/test_a.py", "def test_ok():\n    pass\n\ndef test_bad():\n    assert 0\n"
    )
    g.write("ci_snip/tests/test_b.py", "def test_ok2():\n    pass\n")
    (suite / ".git").mkdir(exist_ok=True)
    fake = g.tmp / "ci_fakebin"
    log = g.tmp / "ci_fake_calls.log"
    _write_exec(fake / "rstest", f'#!/bin/sh\nexec "{binary}" "$@"\n')
    for tool in ("az", "aws", "gsutil", "buildkite-agent", "pip"):
        _write_exec(fake / tool, f'#!/bin/sh\necho "{tool} $*" >> "{log}"\nexit 0\n')
    path = os.pathsep.join([str(fake), os.environ.get("PATH", "")])

    def sh(script, argv, extra=None, cwd=suite):
        f = g.tmp / f"ci_snip_{abs(hash(script)) % 10**8}.sh"
        f.write_text(script, encoding="utf-8")
        env = _base_env(g, {"PATH": path, **(extra or {})})
        return subprocess.run(
            [bash, *argv, str(f)],
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=120,
        )

    # CI-09 Azure "materialize a dir": Azure runs `script:` as a file under
    # `bash --noprofile --norc` with no errexit; $(Var) macros are expanded
    # by the agent before bash sees the script.
    block = _doc_block(DOCS / "guides/ci-recipes.md", "az storage blob download-batch")
    script = block and _yaml_literal(block, r"\s*- script:")
    check(
        "CI-09 setup: Azure materialize-a-dir block found in ci-recipes.md",
        bool(script and "rstest" in script),
        str(block)[:200],
    )
    if script:
        script = script.replace("$(System.JobPositionInPhase)", "1").replace(
            "$(System.TotalJobsInPhase)", "1"
        )
        r = sh(script, ["--noprofile", "--norc"])
        check(
            "CI-09 setup: Azure block ran rstest and az against the failing suite",
            "1 failed" in r.stdout and "upload-batch" in log.read_text(encoding="utf-8"),
            r.stdout[-200:] + r.stderr[-200:],
        )
        check(
            "CI-09 Azure materialize-a-dir: step fails when a test fails",
            r.returncode != 0,
            f"rc={r.returncode}",
            known_bug=True,
        )

    # CI-09 shared-cache "retry without --cache-pull" block, as a GitHub
    # `run:` step (`bash -e {0}`): both the plain failure and the
    # pull-failed fallback must keep the step red.
    block = _doc_block(DOCS / "guides/ci-shared-cache.md", "Error: pulling shared cache", "tee")
    check(
        "CI-09 setup: shared-cache retry block found in ci-shared-cache.md",
        bool(block and "rstest" in block),
        str(block)[:200],
    )
    if block:
        good, bad = g.tmp / "ci_snip_remote", g.tmp / "ci_snip_remote_file"
        good.mkdir(exist_ok=True)
        bad.write_text("x\n", encoding="utf-8")
        for tag, remote in (("reachable remote", good), ("failed pull", bad)):
            r = sh(block + "\n", ["--noprofile", "--norc", "-e"], {"REMOTE": str(remote)})
            ran = "1 failed" in r.stdout
            if tag == "failed pull":
                ran = ran and "Error: pulling shared cache" in r.stdout + r.stderr
            check(
                f"CI-09 shared-cache retry ({tag}): rstest ran, step fails",
                ran and r.returncode != 0,
                f"rc={r.returncode} " + (r.stdout + r.stderr)[-200:],
            )

    # CI-09 GitLab sharding: each `script:` line runs under errexit.
    block = _doc_block(DOCS / "guides/ci-recipes.md", "CI_NODE_INDEX", "parallel:")
    lines = block and _yaml_list(block, "script")
    check(
        "CI-09 setup: GitLab sharding block found in ci-recipes.md",
        bool(lines and any("rstest" in ln for ln in lines)),
        str(block)[:200],
    )
    if lines:
        j = suite / "junit.xml"
        j.unlink(missing_ok=True)
        r = sh(
            "set -eo pipefail\n" + "\n".join(lines) + "\n",
            ["--noprofile", "--norc"],
            {"CI_NODE_INDEX": "1", "CI_NODE_TOTAL": "1", "GITLAB_CI": "true"},
        )
        check(
            "CI-09 GitLab sharding: job fails when a test fails, JUnit written",
            r.returncode != 0 and "1 failed" in r.stdout and j.is_file(),
            f"rc={r.returncode} junit={j.is_file()} " + r.stdout[-200:],
        )

    # CI-10: the bundled action's "Run rstest" step, extracted from
    # action.yml and run as GitHub runs composite bash steps, with a fake
    # rstest that echoes its argv one per line.
    lines = ACTION.read_text(encoding="utf-8").splitlines()
    step = None
    for i, ln in enumerate(lines):
        if ln.strip() == "- name: Run rstest":
            rest = "\n".join(lines[i:])
            step = _yaml_literal(rest, r"\s+run:")
            break
    check(
        "CI-10 setup: 'Run rstest' run step found in action.yml",
        bool(step and "IN_SHARD" in step),
        str(step)[:200],
    )
    if not step:
        return
    echo_bin = g.tmp / "ci_echobin"
    _write_exec(
        echo_bin / "rstest",
        '#!/bin/sh\nfor a in "$@"; do printf \'ARG<%s>\\n\' "$a"; done\nexit "${FAKE_RC:-0}"\n',
    )
    env_names = sorted(set(re.findall(r"\b(IN_[A-Z_]+)\b", step)))
    base = dict.fromkeys(env_names, "")
    base.update(
        {
            "MODE": "plain",
            "BACKEND": "none",
            "REMOTE": "",
            "BASE_REF": "",
            "CACHE_PUSH": "false",
            "IN_CACHE_REMOTE_TOKEN": "",
        }
    )
    epath = os.pathsep.join([str(echo_bin), os.environ.get("PATH", "")])
    outf = g.tmp / "ci_gh_output"

    def action(**inputs):
        outf.write_text("", encoding="utf-8")
        env = {**base, **inputs, "PATH": epath, "GITHUB_OUTPUT": str(outf)}
        r = sh(step, ["--noprofile", "--norc", "-eo", "pipefail"], env, cwd=g.tmp)
        return r, re.findall(r"^ARG<(.*)>$", r.stdout, re.M)

    r, argv = action(IN_SHARD="2", IN_SHARD_TOTAL="4", IN_ARGS='-k "a and b"')
    check(
        "CI-10 setup: shard pair and quoted args reach rstest",
        r.returncode == 0
        and argv[argv.index("--shard") + 1 : argv.index("--shard") + 2] == ["2/4"]
        and "a and b" in argv,
        f"rc={r.returncode} argv={argv}",
    )
    r, _ = action(FAKE_RC="1")
    check(
        "CI-10 rstest exit code propagates to the step",
        r.returncode == 1 and "exit-code=1" in outf.read_text(encoding="utf-8"),
        f"rc={r.returncode}",
    )
    for a, b in (("2", ""), ("", "4")):
        r, argv = action(IN_SHARD=a, IN_SHARD_TOTAL=b)
        check(
            f"CI-10 shard={a!r} shard-total={b!r}: ::error:: and the step fails",
            r.returncode != 0 and "::error::" in r.stdout + r.stderr,
            f"rc={r.returncode} argv={argv}",
            known_bug=True,
        )
    r, argv = action(IN_DOCTOR_FAIL_ON=" wait_pct>50 , wall_seconds > 100 ")
    check(
        "CI-10 doctor-fail-on: comma list trimmed into one flag per condition",
        r.returncode == 0
        and [argv[i + 1] for i, x in enumerate(argv) if x == "--doctor-fail-on"]
        == ["wait_pct>50", "wall_seconds > 100"],
        f"rc={r.returncode} argv={argv}",
    )
    for raw in (r"Connection\s+reset", "can't connect", 'say "hi"'):
        r, argv = action(IN_RERUN_ON=f" {raw} ")
        got = argv[argv.index("--only-rerun") + 1] if "--only-rerun" in argv else None
        check(
            f"CI-10 rerun-on {raw!r}: reaches --only-rerun unmangled",
            r.returncode == 0 and got == raw,
            f"rc={r.returncode} got={got!r} " + r.stderr[-120:],
            known_bug=True,
        )


def _run_pty(cmd, cwd, env, timeout=30):
    """Run cmd on a pseudo-terminal; return (exit code, decoded output)."""
    import pty
    import select

    m, s = pty.openpty()
    p = subprocess.Popen(
        cmd, cwd=cwd, env=env, stdin=s, stdout=s, stderr=s, close_fds=True, start_new_session=True
    )
    os.close(s)
    buf, deadline = b"", time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([m], [], [], 0.2)
            if ready:
                try:
                    data = os.read(m, 65536)
                except OSError:
                    break
                if not data:
                    break
                buf += data
            elif p.poll() is not None:
                break
    finally:
        if p.poll() is None:
            os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        os.close(m)
    return p.returncode, buf.decode("utf-8", "replace")


def _bare_venv(path):
    """A venv with no packages at all (no pip). Returns (python, purelib)."""
    subprocess.run([find_python(), "-m", "venv", "--without-pip", str(path)], check=True)
    py = venv_bin(path, "python")
    return py, _purelib(py)


def _purelib(py):
    return subprocess.run(
        [str(py), "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def gate_ci_environment(g, args, binary):
    print("== ci: non-interactive logs and the pre-commit interpreter (CI-12, CI-13) ==")
    # CI-12: Buildkite (and docker -t) give the job a pty with CI=true.
    for i in range(4):
        g.write(
            f"ci_tty/test_{i}.py",
            "import time\n\n"
            "def test_a():\n    time.sleep(0.4)\n\n"
            "def test_b():\n    time.sleep(0.4)\n\n"
            "def test_c():\n    assert 1 == 2\n",
        )
    cwd = g.tmp / "ci_tty"
    (cwd / ".git").mkdir(exist_ok=True)
    if WINDOWS:
        print("  skip  CI-12 pty checks (POSIX pty)")
    else:
        cmd = [str(binary), "-n", "2"]

        def on_pty(extra_args=(), **env):
            return _run_pty(cmd + list(extra_args), cwd, _base_env(g, {"TERM": "xterm", **env}))

        rc, o = on_pty()
        check(
            "CI-12 setup: interactive pty run draws the live footer",
            rc == 1 and CURSOR_RE.search(o) is not None,
            f"rc={rc} " + repr(o[-200:]),
        )
        rc, o = on_pty(CI="true")
        check(
            "CI-12 CI=true on a pty: no cursor-movement sequences",
            rc == 1 and CURSOR_RE.search(o) is None,
            f"rc={rc} cursor={sorted(set(CURSOR_RE.findall(o)))}",
            known_bug=True,
        )
        rc, o = on_pty(NO_COLOR="1")
        check(
            "CI-12 NO_COLOR=1 on a pty: no SGR color codes",
            rc == 1 and SGR_RE.search(o) is None,
            f"rc={rc} sgr={sorted(set(SGR_RE.findall(o)))}",
            known_bug=True,
        )
        rc, o = on_pty(["--color=no"])
        check(
            "CI-12 --color=no on a pty: no SGR color codes",
            rc == 1 and SGR_RE.search(o) is None,
            f"rc={rc} sgr={sorted(set(SGR_RE.findall(o)))}",
            known_bug=True,
        )

    # Piped (GitHub Actions, GitLab, Jenkins).
    r = g.run("-n", "2", cwd=cwd, env_extra={"CI": "true"})
    o = _out(r)
    check(
        "CI-12 CI=true piped: no cursor movement and no color",
        r.returncode == 1 and CURSOR_RE.search(o) is None and SGR_RE.search(o) is None,
        repr(o[-200:]),
    )
    r = g.run("-n", "2", "--color=yes", cwd=cwd, env_extra={"CI": "true"})
    check(
        "CI-12 --color=yes piped: colored, still no cursor movement",
        SGR_RE.search(r.stdout) is not None and CURSOR_RE.search(_out(r)) is None,
        repr(r.stdout[-200:]),
    )
    py = venv_bin(g.venv, "python")
    pr = subprocess.run(
        [str(py), "-m", "pytest", "-p", "no:cacheprovider"],
        cwd=cwd,
        env=_base_env(g, {"FORCE_COLOR": "1"}),
        capture_output=True,
        text=True,
        timeout=60,
    )
    check(
        "CI-12 setup: pytest colors piped output under FORCE_COLOR=1",
        SGR_RE.search(pr.stdout) is not None,
        repr(pr.stdout[-120:]),
    )
    r = g.run("-n", "2", cwd=cwd, env_extra={"FORCE_COLOR": "1"})
    check(
        "CI-12 FORCE_COLOR=1 piped: output is colored, as with pytest",
        SGR_RE.search(r.stdout) is not None,
        repr(r.stdout[-200:]),
        known_bug=True,
    )

    # CI-13: pre-commit runs `language: python` hooks with VIRTUAL_ENV set
    # to the hook's own env (rstest + pytest, none of the project's deps).
    gate_purelib = _purelib(py)
    proj = g.tmp / "ci_precommit"
    (proj / ".git").mkdir(parents=True, exist_ok=True)
    g.write("ci_precommit/deps/ci_dep.py", "VALUE = 1\n")
    g.write(
        "ci_precommit/tests/test_dep.py",
        "import ci_dep\n\ndef test_dep():\n    assert ci_dep.VALUE == 1\n",
    )
    g.write("ci_precommit/tests/test_more.py", "def test_more():\n    pass\n")
    proj_py, purelib = _bare_venv(proj / ".venv")
    with open(f"{purelib}/ci_paths.pth", "w", encoding="utf-8") as f:
        f.write(gate_purelib + "\n" + str(proj / "deps") + "\n")
    hook = g.tmp / "ci_hook_env"
    _, hook_purelib = _bare_venv(hook)
    with open(f"{hook_purelib}/ci_gate.pth", "w", encoding="utf-8") as f:
        f.write(gate_purelib + "\n")
    r = g.run("-n", "2", cwd=proj, env_drop=("VIRTUAL_ENV",))
    check(
        "CI-13 setup: without VIRTUAL_ENV the project .venv runs the suite",
        r.returncode == 0 and "2 passed" in r.stdout,
        f"rc={r.returncode} " + _out(r)[-300:],
    )
    r = g.run("-n", "2", cwd=proj, env_extra={"VIRTUAL_ENV": str(hook)})
    out = _out(r)
    check(
        "CI-13 setup: hook VIRTUAL_ENV wins discovery and lacks the deps",
        r.returncode != 0 and "ci_dep" in out,
        f"rc={r.returncode} " + out[-300:],
    )
    check(
        "CI-13 hook VIRTUAL_ENV: rstest names the env it used or the skipped .venv",
        str(hook) in out or ".venv" in out,
        out[-300:],
        known_bug=True,
    )
    r = g.run("-n", "2", "--python", str(proj_py), cwd=proj, env_extra={"VIRTUAL_ENV": str(hook)})
    check(
        "CI-13 --python .venv/bin/python overrides the hook VIRTUAL_ENV",
        r.returncode == 0 and "2 passed" in r.stdout,
        f"rc={r.returncode} " + _out(r)[-300:],
    )
    text = (DOCS / "guides/ci-recipes.md").read_text(encoding="utf-8")
    sec = text.split("## Pre-commit", 1)[-1].split("\n## ", 1)[0]
    check(
        "CI-13 docs: pre-commit section shows a hook that uses the project interpreter",
        "language: system" in sec or "--python" in sec,
        sec[:200],
    )
