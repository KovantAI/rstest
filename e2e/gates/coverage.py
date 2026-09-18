"""e2e gate sections: coverage."""

import json
import re
import shutil
import subprocess

from _harness import check, git, git_commit, git_init_commit


def gate_coverage(g, args, binary):
    print("== coverage ==")
    g.write(
        "cov/mypkg/__init__.py",
        "def used(x):\n    return x * 2\n\n\ndef unused(x):\n    return x - 1\n",
    )
    g.write(
        "cov/test_cov.py", "from mypkg import used\n\n\ndef test_used():\n    assert used(2) == 4\n"
    )
    covdir = g.tmp / "cov"
    r = g.run(
        "test_cov.py",
        "-n",
        "2",
        "--cov=mypkg",
        "--cov-report=term",
        cwd=covdir,
        env_extra={"PYTHONPATH": str(covdir)},
    )
    check(
        "coverage report under pool",
        "mypkg" in r.stdout and "__init__.py" in r.stdout and "%" in r.stdout,
        r.stdout[-300:],
    )
    r = g.run(
        "test_cov.py",
        "-n",
        "2",
        "--cov=mypkg",
        "--cov-fail-under=99",
        cwd=covdir,
        env_extra={"PYTHONPATH": str(covdir)},
    )
    check(
        "coverage fail-under exits 1",
        r.returncode == 1 and "FAIL Required" in r.stdout,
        r.stdout[-200:],
    )


def gate_coverage_contexts_line_test_index_cov_co(g, args, binary):
    print("== coverage contexts + line->test index (--cov-context) ==")
    # Per-test contexts must survive the PARALLEL merge (tests land on different
    # workers, yet each covered line keeps its context), and --cov-context must
    # emit the line->test index that coverage-based --changed consumes.
    ctxdir = g.tmp / "covctx"
    g.write(
        "covctx/mymod.py",
        "def used_by_a():\n    return 1\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    g.write(
        "covctx/test_a.py",
        "import mymod\n"
        "def test_a():\n    assert mymod.used_by_a() == 1\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    g.write(
        "covctx/test_b.py",
        "import mymod\n"
        "def test_b():\n    assert mymod.used_by_b() == 2\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    shutil.rmtree(ctxdir / ".rstest_cache", ignore_errors=True)
    r = g.run(
        "test_a.py",
        "test_b.py",
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=ctxdir,
        env_extra={"PYTHONPATH": str(ctxdir)},
    )
    idx_path = ctxdir / ".rstest_cache" / "coverage_index.json"
    check(
        "cov-context: line->test index written",
        r.returncode == 0 and idx_path.exists(),
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    if idx_path.exists():
        idx = json.loads(idx_path.read_text())
        fm = idx.get("files", {}).get("mymod.py", {})
        # schema 2 nests the line map under "lines" and stamps a source "hash".
        lm = fm.get("lines", {})
        # used_by_a body (line 2) only test_a; used_by_b (line 4) only test_b;
        # used_by_both (line 6) BOTH - proving cross-worker context merge.
        check(
            "cov-context: schema + per-test line mapping",
            idx.get("schema") == 2
            and isinstance(fm.get("hash"), str)
            and len(fm["hash"]) == 64
            and lm.get("2") == ["test_a.py::test_a"]
            and lm.get("4") == ["test_b.py::test_b"]
            and lm.get("6") == ["test_a.py::test_a", "test_b.py::test_b"],
            json.dumps(fm),
        )
    # Without --cov-context, no index is written (feature is opt-in via the flag).
    # Wipe the cache AND run 1's leftover coverage data files so this run is
    # hermetic: a stale .coverage from the prior run is a SQLite DB that can lock
    # on Windows when covtool combines into it, flaking this check (rc=1).
    shutil.rmtree(ctxdir / ".rstest_cache", ignore_errors=True)
    for stale in ctxdir.glob(".coverage*"):
        stale.unlink()
    r = g.run(
        "test_a.py",
        "test_b.py",
        "-n",
        "2",
        "--cov=mymod",
        "--cov-report=",
        cwd=ctxdir,
        env_extra={"PYTHONPATH": str(ctxdir)},
    )
    check(
        "cov-context: no index without the flag",
        r.returncode == 0 and not idx_path.exists(),
        f"rc={r.returncode}",
    )


def gate_smart_selection(g, args, binary):
    print("== smart selection ==")
    sp = g.tmp / "selproj"
    g.write("selproj/pkg/__init__.py", "")
    g.write("selproj/pkg/a.py", "def alpha():\n    return 1\n")
    g.write("selproj/pkg/b.py", "def beta():\n    return 2\n")
    g.write("selproj/pkg/c.py", "from .a import alpha\n\n\ndef gamma():\n    return alpha() + 1\n")
    g.write(
        "selproj/tests/test_a.py",
        "from pkg.a import alpha\n\ndef test_alpha(): assert alpha() == 1\n",
    )
    g.write(
        "selproj/tests/test_b.py", "from pkg.b import beta\n\ndef test_beta(): assert beta() == 2\n"
    )
    g.write(
        "selproj/tests/test_c.py",
        "from pkg.c import gamma\n\ndef test_gamma(): assert gamma() == 2\n",
    )
    g.write("selproj/pyproject.toml", '[tool.pytest.ini_options]\ntestpaths = ["tests"]\n')
    git_init_commit(sp, "init")
    with open(sp / "pkg" / "a.py", "a") as f:
        f.write("# touched\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: direct + transitive, excludes unrelated",
        "2 affected test target(s)" in r.stderr
        and "test_alpha" in r.stdout
        and "test_gamma" in r.stdout
        and "test_beta" not in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )
    g.write("selproj/pytest.ini", "[pytest]\n")
    r = g.run("--changed", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: config change -> full run",
        "falling back to full run" in r.stderr,
        r.stderr[-200:],
    )
    (sp / "pytest.ini").unlink()
    git(sp, "add", "-A")
    git_commit(sp, "w")
    r = g.run("--changed", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: clean tree -> nothing",
        r.returncode == 0 and "no tests affected" in r.stdout,
        r.stdout[-200:],
    )
    g.write("selproj/tests/conftest.py", "import pytest\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: conftest -> whole subtree",
        "3 affected test target(s)" in r.stderr and "test_beta" in r.stdout,
        r.stderr[-200:],
    )
    (sp / "tests" / "conftest.py").unlink()
    g.write("selproj/tests/test_new.py", "def test_fresh(): assert True\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: untracked test file selected",
        "test_fresh" in r.stdout and "test_beta" not in r.stdout,
        r.stdout[-200:],
    )
    (sp / "tests" / "test_new.py").unlink()

    # PR-aware --changed: with GITHUB_BASE_REF set, bare --changed diffs vs
    # the merge-base with origin/<base> - a clean checkout of a PR commit
    # still selects the PR's files (vs HEAD it would select nothing).
    base_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=sp, capture_output=True, text=True, check=True
    ).stdout.strip()
    subprocess.run(
        ["git", "update-ref", "refs/remotes/origin/mainline", base_sha], cwd=sp, check=True
    )
    with open(sp / "pkg" / "a.py", "a") as f:
        f.write("# pr change\n")
    git(sp, "add", "-A")
    git_commit(sp, "pr")
    r = g.run(
        "--changed",
        "-v",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "mainline"},
    )
    check(
        "selection: GITHUB_BASE_REF auto-targets PR base",
        "auto-targets PR base origin/mainline" in r.stderr
        and "2 affected test target(s)" in r.stderr
        and "test_alpha" in r.stdout
        and "test_beta" not in r.stdout,
        r.stderr[-300:],
    )
    r = g.run(
        "--changed",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "nosuchbranch"},
    )
    check(
        "selection: missing PR base ref errors, no silent skip",
        r.returncode != 0 and "fetch the base branch" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
    r = g.run(
        "--changed=HEAD~1",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "mainline"},
    )
    check(
        "selection: explicit rev wins over GITHUB_BASE_REF",
        "auto-targets" not in r.stderr and "2 affected test target(s)" in r.stderr,
        r.stderr[-300:],
    )
    # Buildkite exposes the base as a branch name, resolved the same way.
    r = g.run(
        "--changed",
        "-v",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "BUILDKITE_PULL_REQUEST_BASE_BRANCH": "mainline"},
    )
    check(
        "selection: Buildkite base branch auto-targets PR base",
        "auto-targets PR base origin/mainline" in r.stderr and "test_alpha" in r.stdout,
        r.stderr[-300:],
    )
    # GitLab provides the exact diff-base SHA - used directly, no merge-base.
    r = g.run(
        "--changed",
        "-v",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "CI_MERGE_REQUEST_DIFF_BASE_SHA": base_sha},
    )
    check(
        "selection: GitLab diff-base SHA auto-targets MR base",
        "auto-targets MR base" in r.stderr and "test_alpha" in r.stdout,
        r.stderr[-300:],
    )
    # An unresolvable GitLab base SHA errors - never a silent full skip.
    r = g.run(
        "--changed",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "CI_MERGE_REQUEST_DIFF_BASE_SHA": "0" * 40},
    )
    check(
        "selection: missing GitLab base SHA errors, no silent skip",
        r.returncode != 0 and "not in the local clone" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )


def gate_coverage_based_selection_changed_uses_th(g, args, binary):
    print("== coverage-based selection (--changed uses the cov index) ==")
    # Warm a line->test index, then prove --changed narrows to only the tests
    # whose recorded coverage hit the changed lines - tighter than the
    # import-graph (which would run every test importing the module).
    cs = g.tmp / "covsel"
    g.write(
        "covsel/mymod.py",
        "def used_by_a():\n    return 1\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    g.write(
        "covsel/test_a.py",
        "import mymod\n"
        "def test_a():\n    assert mymod.used_by_a() == 1\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    g.write(
        "covsel/test_b.py",
        "import mymod\n"
        "def test_b():\n    assert mymod.used_by_b() == 2\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    g.write("covsel/pyproject.toml", "[tool.pytest.ini_options]\n")
    git_init_commit(cs, "init")

    def cov_changed_targets(edit_fn):
        # reset, apply edit to the working tree, list the selected nodeids
        git(cs, "checkout", "-q", ".")
        edit_fn()
        r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
        got = sorted(set(re.findall(r"test_[ab]\.py::test_[ab]", r.stdout)))
        return got, r

    def edit_line(path, old, new):
        p = cs / path
        p.write_text(p.read_text().replace(old, new), encoding="utf-8")

    # Warm the index (writes covsel/.rstest_cache/coverage_index.json).
    r = g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )
    check(
        "cov-select: index warmed",
        (cs / ".rstest_cache" / "coverage_index.json").exists(),
        r.stdout[-200:] + r.stderr[-200:],
    )

    got, r = cov_changed_targets(
        lambda: edit_line("mymod.py", "    return 1\n", "    return 111\n")
    )
    check(
        "cov-select: edit used_by_a -> only test_a",
        got == ["test_a.py::test_a"],
        f"{got} || {r.stderr[-150:]}",
    )

    got, _ = cov_changed_targets(lambda: edit_line("mymod.py", "    return 2\n", "    return 22\n"))
    check("cov-select: edit used_by_b -> only test_b", got == ["test_b.py::test_b"], str(got))

    got, _ = cov_changed_targets(lambda: edit_line("mymod.py", "    return 3\n", "    return 33\n"))
    check(
        "cov-select: edit shared line -> both tests",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: a pure insertion (new function) has no old-side coverage -> falls
    # back to import-graph, which runs every test importing the module.
    got, _ = cov_changed_targets(
        lambda: (cs / "mymod.py").write_text(
            (cs / "mymod.py").read_text() + "\ndef brand_new():\n    return 9\n", encoding="utf-8"
        )
    )
    check(
        "cov-select: new code falls back to import-graph (both)",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: a changed test file always runs its own tests.
    got, _ = cov_changed_targets(lambda: edit_line("test_a.py", "== 1\n", "== 1  # x\n"))
    check("cov-select: changed test file runs itself", got == ["test_a.py::test_a"], str(got))

    # Rail: a `def` line runs at import time under the empty context, so it is
    # never in the index. Trusting the empty lookup would select ZERO tests;
    # instead the file must fall back to import-graph (both tests).
    got, _ = cov_changed_targets(
        lambda: edit_line("mymod.py", "def used_by_a():\n", "def used_by_a(x=1):\n")
    )
    check(
        "cov-select: edited def line falls back to import-graph (both)",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: cold cache (no index) is identical to import-graph selection.
    shutil.rmtree(cs / ".rstest_cache", ignore_errors=True)
    got, _ = cov_changed_targets(
        lambda: edit_line("mymod.py", "    return 1\n", "    return 111\n")
    )
    check(
        "cov-select: cold cache -> import-graph (both)",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: a warm index may hold a nodeid for a test renamed since it was built.
    # Passing the stale nodeid aborts the run, and dropping it skips a test still
    # covering the changed line, so the file demotes to import-graph instead.
    git(cs, "checkout", "-q", ".")
    git(cs, "clean", "-fdq")
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )  # warm index (knows test_a.py::test_a)
    git(cs, "mv", "test_a.py", "test_renamed.py")
    edit_line("mymod.py", "    return 1\n", "    return 111\n")  # line only test_a covered
    r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    got = sorted(set(re.findall(r"test_\w+\.py::test_\w+", r.stdout)))
    check(
        "cov-select: renamed test (stale nodeid) -> no crash, runs via fallback",
        r.returncode == 0 and "test_renamed.py::test_a" in got and "not found" not in r.stdout,
        f"rc={r.returncode} {got} {r.stderr[-150:]}",
    )
    git(cs, "reset", "-q", "--hard", "HEAD")
    git(cs, "clean", "-fdq")

    # Rail: an index warmed before a line-shifting commit must not be trusted at
    # stale lines. A prepend shifts used_by_a's body; the per-file SHA-256 no
    # longer matches HEAD, so the file drifts to import-graph, not a stale lookup.
    git(cs, "checkout", "-q", ".")
    git(cs, "clean", "-fdq")
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )  # warm at current HEAD
    (cs / "mymod.py").write_text(
        "def zzz():\n    return 0\n" + (cs / "mymod.py").read_text(), encoding="utf-8"
    )
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qam",
            "prepend shifts lines",
        ],
        cwd=cs,
        check=True,
    )
    edit_line("mymod.py", "    return 1\n", "    return 111\n")  # used_by_a body, now shifted
    r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    got = sorted(set(re.findall(r"test_[ab]\.py::test_[ab]", r.stdout)))
    check(
        "cov-select: line-shift drift -> import-graph fallback, not stale lookup",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        f"{got} {r.stderr[-150:]}",
    )
    git(cs, "reset", "-q", "--hard", "HEAD~1")
    git(cs, "clean", "-fdq")

    print("== changed selection: files with no line-diff ==")
    # `git diff -U0` emits no hunk for deletions, binaries, and renames, so
    # parse_diff_hunks drops them; a --name-only union recovers them for the
    # fallback. A deleted test file must be skipped, not handed to pytest.
    git(cs, "reset", "-q", "--hard", "HEAD")
    git(cs, "clean", "-fdq")
    # Warm the index so the deleted-test case exercises the warm direct_tests
    # branch (the path actually wired for coverage selection).
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )

    # Deleted TEST file: no file on disk -> dropped, not selected. Nothing else
    # changed -> nothing to run, and crucially NO missing-path error.
    (cs / "test_a.py").unlink()
    r = g.run("--changed", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    check(
        "changed: deleted test file skipped, no missing-path error",
        r.returncode == 0
        and "no tests affected" in r.stdout
        and "not found" not in (r.stdout + r.stderr)
        and "No such file" not in (r.stdout + r.stderr),
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-250:],
    )
    git(cs, "checkout", "-q", ".")

    # Deleted SOURCE file under --changed-strict: -U0 shows +++ /dev/null (no
    # hunk). Pre-fix it was dropped and falsely SKIPPED everything; the
    # --name-only union routes it to the strict rail, forcing a full run.
    (cs / "mymod.py").unlink()
    r = g.run("--changed-strict", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    check(
        "changed-strict: deleted source file forces full run (not a false skip)",
        "falling back to full run" in r.stderr and "no tests affected" not in r.stdout,
        r.stderr[-250:] + " || " + r.stdout[-150:],
    )
    git(cs, "checkout", "-q", ".")

    # Changed BINARY (non-Python) file: -U0 emits no @@ hunk, so parse_diff_hunks
    # drops it; the --name-only union recovers it and rule1 forces a full run.
    (cs / "blob.bin").write_bytes(b"\x00\x01\x02rstest\x00")
    git(cs, "add", "-A")
    git_commit(cs, "add binary")
    (cs / "blob.bin").write_bytes(b"\x00\x01\x02rstest\xffCHANGED\x00")
    r = g.run("--changed", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    check(
        "changed: modified binary file -> full run (name-only recovers it)",
        "falling back to full run" in r.stderr and "non-Python" in r.stderr,
        r.stderr[-250:],
    )
    git(cs, "reset", "-q", "--hard", "HEAD~1")
    git(cs, "clean", "-fdq")


def gate_coverage_selection_under_autocrlf_crlf_w(g, args, binary):
    print("== coverage selection under autocrlf (CRLF worktree, LF blob) ==")
    # Regression: autocrlf stores the blob LF while the worktree is CRLF. The
    # index hash (worktree, Python) and drift check (blob, Rust) must agree once
    # both normalize newlines, else every indexed file "drifts" on Windows.
    cr = g.tmp / "crlf"
    cr.mkdir(parents=True, exist_ok=True)

    def wr_crlf(rel, text):
        (cr / rel).write_bytes(text.replace("\n", "\r\n").encode("utf-8"))

    wr_crlf(
        "mymod.py",
        "def used_by_a():\n    return 1\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    wr_crlf(
        "test_a.py",
        "import mymod\n"
        "def test_a():\n    assert mymod.used_by_a() == 1\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    wr_crlf(
        "test_b.py",
        "import mymod\n"
        "def test_b():\n    assert mymod.used_by_b() == 2\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    (cr / "pyproject.toml").write_bytes(b"[tool.pytest.ini_options]\n")
    git(cr, "init", "-q")
    git(cr, "config", "core.autocrlf", "true")
    subprocess.run(["git", "add", "-A"], cwd=cr, check=True, capture_output=True)  # blobs -> LF
    git_commit(cr, "init")
    blob = subprocess.run(["git", "show", "HEAD:mymod.py"], cwd=cr, capture_output=True).stdout
    check(
        "autocrlf: blob normalized to LF, worktree stays CRLF",
        b"\r\n" not in blob and b"\r\n" in (cr / "mymod.py").read_bytes(),
        f"blob_has_crlf={b'/r/n' in blob}",
    )
    # Warm the index (Python hashes the CRLF worktree, normalized to LF).
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cr,
        env_extra={"PYTHONPATH": str(cr)},
    )
    # Edit only used_by_a's body. The stored hash (normalized CRLF) still equals
    # the base blob hash (normalized LF), so the index is trusted and narrows to
    # test_a. Pre-fix the CRLF-vs-LF mismatch drifted every file, running both.
    wr_crlf(
        "mymod.py",
        "def used_by_a():\n    return 111\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cr, env_extra={"PYTHONPATH": str(cr)})
    got = sorted(set(re.findall(r"test_[ab]\.py::test_[ab]", r.stdout)))
    check(
        "autocrlf: index trusted across CRLF/LF -> narrows to test_a",
        got == ["test_a.py::test_a"],
        f"{got} || {r.stderr[-150:]}",
    )


def gate_diff_coverage_gate(g, args, binary):
    print("== diff coverage gate (--cov-diff-fail-under) ==")
    dp = g.tmp / "diffcov"
    g.write("diffcov/mod.py", "def used():\n    return 1\n")
    g.write("diffcov/test_mod.py", "import mod\ndef test_used():\n    assert mod.used() == 1\n")
    git_init_commit(dp, "base")
    # Add a function with a covered branch and an UNcovered branch; cover only
    # the first from a new test.
    g.write(
        "diffcov/mod.py",
        "def used():\n    return 1\n\n"
        "def added(flag):\n    if flag:\n"
        '        return "yes"\n    return "no"\n',
    )
    g.write(
        "diffcov/test_mod.py",
        "import mod\n"
        "def test_used():\n    assert mod.used() == 1\n"
        'def test_added():\n    assert mod.added(True) == "yes"\n',
    )
    env = {"PYTHONPATH": str(dp)}
    cov = ["--cov=.", "--cov-report="]

    r = g.run("-n", "2", *cov, "--cov-diff-fail-under", "100", cwd=dp, env_extra=env)
    check(
        "diff-cov: uncovered added line fails the gate + is named",
        r.returncode == 1
        and "is below 100%" in r.stderr
        and "uncovered added line" in r.stdout
        and "mod.py" in r.stdout,
        f"rc={r.returncode} " + r.stderr[-200:] + r.stdout[-200:],
    )
    # A lower bar (83% covered) passes at --cov-diff-fail-under 80.
    r = g.run("-n", "2", *cov, "--cov-diff-fail-under", "80", cwd=dp, env_extra=env)
    check(
        "diff-cov: partial coverage passes a lower threshold",
        r.returncode == 0 and "meets 80%" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # Cover the other branch -> 100%, gate passes.
    g.write(
        "diffcov/test_mod.py",
        "import mod\n"
        "def test_used():\n    assert mod.used() == 1\n"
        "def test_added():\n"
        '    assert mod.added(True) == "yes"\n'
        '    assert mod.added(False) == "no"\n',
    )
    r = g.run("-n", "2", *cov, "--cov-diff-fail-under", "100", cwd=dp, env_extra=env)
    check(
        "diff-cov: fully-covered diff passes at 100%",
        r.returncode == 0 and "diff coverage 100.0% meets 100%" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # Without --cov there's no coverage data to score: the gate is ignored with
    # a warning and does not fail the run.
    r = g.run("-n", "2", "--cov-diff-fail-under", "100", cwd=dp, env_extra=env)
    check(
        "diff-cov: --cov-diff-fail-under without --cov warns and is ignored",
        r.returncode == 0 and "needs --cov" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
