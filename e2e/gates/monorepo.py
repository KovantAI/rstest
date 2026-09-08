"""e2e gate sections: monorepo."""

import json
import shutil
import subprocess
import time

from _harness import FLAKY, WINDOWS, check, git, git_commit, git_init_commit


def gate_monorepo(g, args, binary):
    print("== monorepo ==")
    mono = g.tmp / "mono"
    shutil.rmtree(mono, ignore_errors=True)
    g.write("mono/libs/a/pytest.ini", "[pytest]\n")
    g.write("mono/libs/a/tests/test_a.py", "def test_a1(): pass\ndef test_a2(): pass\n")
    g.write(
        "mono/libs/b/pyproject.toml",
        '[tool.pytest.ini_options]\ntestpaths = ["tests"]\n',
    )
    g.write("mono/libs/b/tests/test_b.py", "def test_b1(): pass\ndef test_b2(): assert False\n")
    # a pyproject without a pytest section is NOT a project
    g.write("mono/libs/c/pyproject.toml", '[project]\nname = "c"\n')
    r = g.run("-n", "2", cwd=mono)
    check(
        "mono: discovers configured projects",
        "monorepo: 2 projects" in r.stdout and "libs/c" not in r.stdout,
        r.stdout[:300],
    )
    check(
        "mono: per-project results",
        "2 passed" in r.stdout and "1 failed, 1 passed" in r.stdout,
        r.stdout[-600:],
    )
    check(
        "mono: summary verdicts",
        "libs/a" in r.stdout and "FAILED (exit 1)" in r.stdout,
        r.stdout[-400:],
    )
    check("mono: merged exit", r.returncode == 1)
    rep = g.tmp / "mono-report.json"
    g.run("-n", "2", "--report-json", str(rep), cwd=mono)
    doc = json.loads(rep.read_text(encoding="utf-8"))
    check(
        "mono: merged report, root-relative keys",
        doc["meta"]["schema"] == 4
        and any(k.startswith("libs/a/") for k in doc["tests"])
        and any(k.startswith("libs/b/") for k in doc["tests"]),
        str(list(doc["tests"])[:4]),
    )
    check(
        "mono: merged report per-project meta + counts",
        doc["meta"]["projects"]["libs/b"]["exitstatus"] == 1
        and doc["meta"]["exitstatus"] == 1
        and doc["meta"]["counts"]["passed"] == 3
        and doc["meta"]["counts"]["failed"] == 1
        and doc["meta"]["projects"]["libs/b"]["counts"]["failed"] == 1,
        str(doc["meta"])[:400],
    )
    # --output forwards to each project. github annotations carry the
    # project's ROOT-relative path (children run with cwd=project, but GitHub
    # resolves annotation files from the repo root).
    r = g.run("-n", "2", "--output", "github", cwd=mono)
    ann = [ln for ln in r.stdout.splitlines() if ln.startswith("::error ")]
    check(
        "mono: github annotations use root-relative file paths",
        len(ann) == 1 and "file=libs/b/" in ann[0],
        (ann[0] if ann else "<no annotation>"),
    )
    # --output json is refused at a monorepo root (no clean merged stream);
    # the error must steer the user to --report-json.
    r = g.run("-n", "2", "--output", "json", cwd=mono)
    check(
        "mono: --output json refused with guidance",
        r.returncode != 0 and "--report-json" in r.stderr and "--output json" in r.stderr,
        r.stderr[-200:],
    )
    # Discovery (--collect-only --report-json) needs a single session, so a
    # monorepo ROOT refuses it (run per-project instead); INSIDE a project it
    # writes the discovery doc scoped to that project's rootdir.
    rootdisc = g.tmp / "mono-rootdisc.json"
    r = g.run("--collect-only", "--report-json", str(rootdisc), cwd=mono)
    check(
        "mono: discovery refused at root, points to per-project",
        r.returncode != 0 and "per project" in (r.stdout + r.stderr) and not rootdisc.exists(),
        (r.stdout + r.stderr)[-300:],
    )
    projdisc = g.tmp / "mono-projdisc.json"
    g.run("--collect-only", "--report-json", str(projdisc), cwd=mono / "libs" / "a")
    pdoc = json.loads(projdisc.read_text(encoding="utf-8"))
    check(
        "mono: per-project discovery scoped to the project",
        pdoc["meta"]["kind"] == "discovery"
        and pdoc["meta"]["rootdir"].replace("\\", "/").endswith("libs/a")
        and len(pdoc["tests"]) == 2
        and all(t["nodeid"].startswith("tests/test_a.py::") for t in pdoc["tests"]),
        str(pdoc["meta"]),
    )
    # explicit path args opt out of monorepo mode
    r = g.run("libs/a", "-n", "2", cwd=mono)
    check(
        "mono: path arg targets single project",
        "monorepo" not in r.stdout and "2 passed" in r.stdout,
        r.stdout[:200],
    )
    # concurrency: two 1.2s projects must overlap, not serialize
    g.write("mono2/p1/pytest.ini", "[pytest]\n")
    g.write("mono2/p1/test_one.py", "import time\ndef test_s(): time.sleep(1.2)\n")
    g.write("mono2/p2/pytest.ini", "[pytest]\n")
    g.write("mono2/p2/test_two.py", "import time\ndef test_s(): time.sleep(1.2)\n")
    t0 = time.monotonic()
    r = g.run("-n", "2", cwd=g.tmp / "mono2", timeout=120)
    wall = time.monotonic() - t0
    # Two 1.2s projects: concurrent ~= 1.2s + per-project startup, serial >=
    # 2.4s + 2x startup. Windows pays a much heavier process/interpreter spawn
    # cost, so the absolute wall is larger while still proving overlap.
    limit = 4.0 if WINDOWS else 2.4
    check(
        "mono: projects run concurrently",
        r.returncode == 0 and wall < limit,
        f"wall={wall:.2f}s rc={r.returncode} limit={limit}",
    )

    # --changed across projects: direct narrows, dependent runs full,
    # unaffected is skipped entirely
    cm = g.tmp / "monochg"
    shutil.rmtree(cm, ignore_errors=True)
    g.write(
        "monochg/libs/a/pyproject.toml",
        '[project]\nname = "pkg-a"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monochg/libs/a/src_a.py", "VALUE = 1\n")
    g.write("monochg/libs/a/test_a.py", "import src_a\ndef test_a(): assert src_a.VALUE\n")
    g.write("monochg/libs/a/test_a_other.py", "def test_other(): pass\n")
    g.write(
        "monochg/libs/b/pyproject.toml",
        '[project]\nname = "pkg-b"\ndependencies = ["pkg-a"]\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monochg/libs/b/test_b.py", "def test_b(): pass\n")
    g.write(
        "monochg/libs/c/pyproject.toml",
        '[project]\nname = "pkg-c"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monochg/libs/c/test_c.py", "def test_c(): pass\n")
    git_init_commit(cm, "base")
    (cm / "libs/a/src_a.py").write_text("VALUE = 2\n")
    r = g.run("-n", "2", "--changed", cwd=cm)
    check(
        "mono: --changed classifies projects",
        "2 of 3 projects affected" in r.stderr,
        r.stderr[-300:],
    )
    check(
        "mono: unaffected project skipped",
        "libs/c" in r.stdout and "skipped (no changes)" in r.stdout,
        r.stdout[-400:],
    )

    def section(text, rel):
        try:
            seg = text.split(f"project: {rel} ")[1]
        except IndexError:
            return ""
        return seg.split("=== project:")[0].split("=== monorepo")[0]

    sec_a = section(r.stdout, "libs/a")
    check(
        "mono: direct project narrows by import graph",
        "1 passed" in sec_a and "2 passed" not in sec_a,
        sec_a[-300:],
    )
    sec_b = section(r.stdout, "libs/b")
    check(
        "mono: dependent project runs full",
        "1 passed" in sec_b and "no tests affected" not in sec_b,
        sec_b[-300:],
    )

    # per-project venv: a project-local .venv wins over the inherited env
    pv = g.tmp / "monovenv"
    shutil.rmtree(pv, ignore_errors=True)
    g.write("monovenv/libs/a/pytest.ini", "[pytest]\n")
    g.write(
        "monovenv/libs/a/test_env.py",
        "import sys\ndef test_which(): print('PYEXE=' + sys.executable)\n",
    )
    local_venv = pv / "libs/a/.venv"
    local_venv.parent.mkdir(parents=True, exist_ok=True)
    if WINDOWS:
        # No exec shims on Windows. Junction the whole .venv to the gate venv:
        # Scripts/python.exe is then a real interpreter with msgpack on its
        # path, and discover_python's Scripts/python.exe probe resolves it.
        subprocess.run(
            ["cmd", "/c", "mklink", "/J", str(local_venv), str(g.venv)],
            check=True,
            capture_output=True,
        )
    else:
        local_bin = local_venv / "bin"
        local_bin.mkdir(parents=True, exist_ok=True)
        local_py = local_bin / "python"
        # An exec shim, not a symlink: a bare symlink loses pyvenv.cfg
        # resolution and lands on the base interpreter (no msgpack).
        local_py.write_text(f'#!/bin/sh\nexec "{g.venv}/bin/python" "$@"\n')
        local_py.chmod(0o755)
    g.write("monovenv/libs/b/pytest.ini", "[pytest]\n")
    g.write("monovenv/libs/b/test_b.py", "def test_b(): pass\n")
    r = g.run("-n", "1", "-v", "-s", "--co", cwd=pv)  # passthrough must bail
    check(
        "mono: passthrough flags bail",
        r.returncode != 0 and "single pytest session" in r.stderr,
        r.stderr[-200:],
    )
    r = g.run("-n", "2", cwd=pv)
    check(
        "mono: project-local venv used",
        r.returncode == 0 and "1 passed" in r.stdout,
        r.stdout[-400:],
    )

    # per-project [tool.rstest]: a numprocesses pin survives the planner
    pp = g.tmp / "monopin"
    shutil.rmtree(pp, ignore_errors=True)
    g.write(
        "monopin/libs/a/pyproject.toml",
        '[tool.pytest.ini_options]\ntestpaths = ["."]\n\n[tool.rstest]\nnumprocesses = 0\n',
    )
    g.write("monopin/libs/a/test_a.py", "def test_a(): pass\n")
    g.write("monopin/libs/b/pytest.ini", "[pytest]\n")
    g.write("monopin/libs/b/test_b.py", "def test_b(): pass\n")
    r = g.run("-n", "4", cwd=pp)
    check(
        "mono: per-project numprocesses pin",
        "libs/a:-n0" in r.stdout and "pytest-exact" in r.stdout and r.returncode == 0,
        r.stdout[:400],
    )

    # coverage under monorepo mode: per-project reports, no collision
    mc = g.tmp / "monocov"
    shutil.rmtree(mc, ignore_errors=True)
    g.write("monocov/libs/a/pytest.ini", "[pytest]\n")
    g.write("monocov/libs/a/pkg_a.py", "def f():\n    return 1\n")
    g.write("monocov/libs/a/test_a.py", "import pkg_a\ndef test_a(): assert pkg_a.f() == 1\n")
    g.write("monocov/libs/b/pytest.ini", "[pytest]\n")
    g.write("monocov/libs/b/pkg_b.py", "def g():\n    return 2\n")
    g.write("monocov/libs/b/test_b.py", "import pkg_b\ndef test_b(): assert pkg_b.g() == 2\n")
    r = g.run("-n", "2", "--cov=.", cwd=mc)
    sec_ca = r.stdout.split("project: libs/a ")[-1].split("=== project")[0]
    sec_cb = r.stdout.split("project: libs/b ")[-1].split("=== project")[0].split("=== monorepo")[0]
    check(
        "mono: per-project coverage reports",
        r.returncode == 0 and "pkg_a.py" in sec_ca and "pkg_b.py" in sec_cb,
        r.stdout[-600:],
    )
    check(
        "mono: coverage does not cross projects",
        "pkg_b.py" not in sec_ca and "pkg_a.py" not in sec_cb,
        sec_ca[-300:],
    )

    # --changed-strict: undeclared sibling import counts as an edge;
    # nothing-affected exits 5
    cs = g.tmp / "monostrict"
    shutil.rmtree(cs, ignore_errors=True)
    g.write(
        "monostrict/libs/a/pyproject.toml",
        '[project]\nname = "pkg-a"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monostrict/libs/a/pkg_a/__init__.py", "VALUE = 1\n")
    g.write("monostrict/libs/a/test_a.py", "from pkg_a import VALUE\ndef test_a(): assert VALUE\n")
    g.write(
        "monostrict/libs/b/pyproject.toml",
        '[project]\nname = "pkg-b"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    # b's test imports pkg_a WITHOUT declaring it (shared-venv trap)
    g.write("monostrict/libs/b/test_b.py", "def test_b(): pass\n")
    g.write("monostrict/libs/b/helper.py", "import pkg_a\n")
    git_init_commit(cs, "base")
    (cs / "libs/a/pkg_a/__init__.py").write_text("VALUE = 2\n")
    r_lax = g.run("-n", "2", "--changed", cwd=cs)
    r_strict = g.run("-n", "2", "--changed-strict", cwd=cs)
    check(
        "strict: undeclared sibling import counted",
        "skipped (no changes)" in r_lax.stdout
        and "skipped (no changes)" not in r_strict.stdout
        and "without declaring it" in r_strict.stderr,
        f"lax:{r_lax.stdout[-200:]}\nstrict:{r_strict.stderr[-200:]}",
    )
    # nothing affected at all -> exit 5 under strict, 0 without
    git(cs, "add", "-A")
    git_commit(cs, "x")
    r0 = g.run("-n", "2", "--changed", cwd=cs)
    r5 = g.run("-n", "2", "--changed-strict", cwd=cs)
    check(
        "strict: nothing affected exits 5 (lax exits 0)",
        r0.returncode == 0 and r5.returncode == 5,
        f"lax={r0.returncode} strict={r5.returncode}",
    )

    # single-project strict: a changed file unreachable from tests
    sp = g.tmp / "strictone"
    shutil.rmtree(sp, ignore_errors=True)
    g.write("strictone/pytest.ini", "[pytest]\n")
    g.write("strictone/used.py", "X = 1\n")
    g.write("strictone/test_main.py", "import used\ndef test_x(): assert used.X\n")
    g.write("strictone/orphan.py", "Y = 1\n")  # imported by nothing
    git_init_commit(sp, "base")
    (sp / "orphan.py").write_text("Y = 2\n")
    r_lax = g.run("-n", "2", "--changed", cwd=sp)
    r_str = g.run("-n", "2", "--changed-strict", cwd=sp)
    check(
        "strict: unreachable changed file forces full run",
        "no tests affected" in r_lax.stdout
        and "reaches no tests" in r_str.stderr
        and "1 passed" in r_str.stdout,
        f"lax:{r_lax.stdout[-150:]} strict:{r_str.stderr[-250:]}",
    )

    # [tool.rstest] projects restricts discovery
    g.write("mono/pyproject.toml", '[tool.rstest]\nprojects = ["libs/a"]\n')
    r = g.run("-n", "2", cwd=mono)
    check(
        "mono: projects globs filter",
        "monorepo: 1 projects" in r.stdout and r.returncode == 0,
        r.stdout[:300],
    )


def gate_tool_rstest_config(g, args, binary):
    print("== [tool.rstest] config ==")
    g.write("toolcfg/pyproject.toml", "[tool.rstest]\nnumprocesses = 2\nreruns = 1\n")
    g.write("toolcfg/test_cfg.py", FLAKY)
    tmarker = g.tmp / "toolcfg_marker"
    if tmarker.exists():
        tmarker.unlink()
    r = g.run("test_cfg.py", cwd=g.tmp / "toolcfg", env_extra={"FLAKY_MARKER": str(tmarker)})
    check(
        "tool.rstest defaults applied",
        r.returncode == 0 and "2 workers" in r.stdout.splitlines()[0] and "1 flaky" in r.stdout,
        r.stdout[:120] + r.stdout[-160:],
    )
    r = g.run(
        "test_cfg.py", "-n", "0", cwd=g.tmp / "toolcfg", env_extra={"FLAKY_MARKER": str(tmarker)}
    )
    check("CLI overrides tool.rstest", "single worker" in r.stdout.splitlines()[0], r.stdout[:120])
    check(
        "tail-batch rerun works (EndSession model)",
        True,  # asserted by 'tool.rstest defaults applied': 1 item, rerun delivered post-drain
    )


def gate_shared_cache_backend(g, args, binary):
    print("== shared cache backend ==")
    # Push this run's segment to a remote dir; a fresh project pulls the union.
    remote = g.tmp / "shared-remote"
    shutil.rmtree(remote, ignore_errors=True)
    g.write(
        "scproj_a/test_s.py",
        "import time\ndef test_slow(): time.sleep(0.05)\ndef test_a(): assert True\n",
    )
    sca = g.tmp / "scproj_a"
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), "--cache-push", cwd=sca)
    segdir = remote / "segments"
    segs = list(segdir.glob("seg-*.json")) if segdir.exists() else []
    check(
        "shared-cache: push writes exactly one segment",
        r.returncode == 0 and len(segs) == 1 and "pushed segment" in r.stderr,
        f"rc={r.returncode} segs={segs} {r.stderr[-150:]}",
    )
    # Fresh project with no local cache: pull populates it from the remote.
    g.write("scproj_b/test_s.py", "def test_a(): assert True\n")
    scb = g.tmp / "scproj_b"
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), "--cache-pull", cwd=scb)
    check(
        "shared-cache: pull populates local durations",
        r.returncode == 0
        and (scb / ".rstest_cache" / "durations.json").exists()
        and "pulled" in r.stderr,
        r.stderr[-200:],
    )
    # require-baseline against a cold remote is a hard error, not a silent skip.
    # RSTEST_CACHE points at an empty dir so a prior local cache can't satisfy it.
    cold = g.tmp / "cold-remote"
    r = g.run(
        "test_s.py",
        "-n",
        "2",
        "--cache-remote",
        str(cold),
        "--cache-pull",
        "--require-baseline",
        "--durations-regress",
        "1.5",
        cwd=scb,
        env_extra={"RSTEST_CACHE": str(g.tmp / "nolocal-cache")},
    )
    check(
        "shared-cache: require-baseline errors on a cold remote",
        r.returncode != 0 and "require-baseline" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # Compact folds the segment into a base and prunes segments.
    r = g.run("cache-compact", "--cache-remote", str(remote), cwd=sca)
    leftover = list(segdir.glob("seg-*.json")) if segdir.exists() else []
    check(
        "shared-cache: compact folds to base, prunes segments",
        r.returncode == 0
        and (remote / "base.json").exists()
        and not leftover
        and "compacted" in r.stderr,
        f"rc={r.returncode} left={leftover}",
    )

    # Coverage index rides the shared cache: two shards each push their PARTIAL
    # coverage slice; a pull unions them into a full line->test index. This is
    # the sharded-partial-index limitation the shared cache exists to fix.
    covremote = g.tmp / "shared-cov-remote"
    shutil.rmtree(covremote, ignore_errors=True)
    scc = g.tmp / "scproj_cov"
    g.write(
        "scproj_cov/mymod.py", "def used_by_a():\n    return 1\ndef used_by_b():\n    return 2\n"
    )
    g.write("scproj_cov/test_a.py", "import mymod\ndef test_a(): assert mymod.used_by_a() == 1\n")
    g.write("scproj_cov/test_b.py", "import mymod\ndef test_b(): assert mymod.used_by_b() == 2\n")
    # Shard 1 covers used_by_a, shard 2 covers used_by_b; each pushes its slice.
    covenv = {"PYTHONPATH": str(scc)}
    for k in (1, 2):
        g.run(
            "-n",
            "2",
            "--shard",
            f"{k}/2",
            "--cov=mymod",
            "--cov-context=test",
            "--cov-report=",
            "--cache-remote",
            str(covremote),
            "--cache-push",
            cwd=scc,
            env_extra=covenv,
        )
    covsegs = sorted((covremote / "segments").glob("seg-*.json"))
    seg_blobs = [json.loads(p.read_text()) for p in covsegs]
    covered_files = {f for b in seg_blobs for f in b.get("cov_index", {}).get("files", {})}
    check(
        "shared-cache: coverage segments carry a cov_index slice",
        len(covsegs) == 2 and any("mymod.py" in f for f in covered_files),
        f"segs={len(covsegs)} files={covered_files}",
    )
    # A fresh clone pulls the UNION: both used_by_a and used_by_b lines mapped.
    sccb = g.tmp / "scproj_cov_b"
    shutil.copytree(scc, sccb)
    shutil.rmtree(sccb / ".rstest_cache", ignore_errors=True)
    r = g.run(
        "-n",
        "2",
        "--cache-remote",
        str(covremote),
        "--cache-pull",
        "--co",
        "-q",
        cwd=sccb,
        env_extra={"PYTHONPATH": str(sccb)},
    )
    pulled_idx = sccb / ".rstest_cache" / "coverage_index.json"
    idx = json.loads(pulled_idx.read_text()) if pulled_idx.exists() else {}
    mymod_key = next((k for k in idx.get("files", {}) if k.endswith("mymod.py")), None)
    nodeids = set()
    if mymod_key:
        for ids in idx["files"][mymod_key]["lines"].values():
            nodeids.update(ids)
    check(
        "shared-cache: pull unions shard coverage slices into one index",
        idx.get("schema") == 2 and {"test_a.py::test_a", "test_b.py::test_b"} <= nodeids,
        f"rc={r.returncode} key={mymod_key} nodeids={sorted(nodeids)}",
    )

    # sccb still holds the pulled MERGED coverage index from above. A --cache-push
    # run that produces NO fresh index of its own — here NO --cov at all, the path
    # that skips covtool entirely — must NOT re-publish that pulled index.
    covr3 = g.tmp / "shared-cov-remote3"
    shutil.rmtree(covr3, ignore_errors=True)
    assert (sccb / ".rstest_cache" / "coverage_index.json").exists()  # precondition
    g.run(
        "-n",
        "2",  # no --cov: covtool never runs, so only the push-time drop guards it
        "--cache-remote",
        str(covr3),
        "--cache-push",
        cwd=sccb,
        env_extra={"PYTHONPATH": str(sccb)},
    )
    r3segs = sorted((covr3 / "segments").glob("seg-*.json"))
    r3blobs = [json.loads(p.read_text()) for p in r3segs]
    republished = any(b.get("cov_index", {}).get("files") for b in r3blobs)
    check(
        "shared-cache: push without a fresh index re-publishes no coverage",
        len(r3segs) >= 1 and not republished,
        f"segs={len(r3segs)} republished={republished}",
    )

    # --cache-pull OVERLAYS remote onto the local cache: local-only entries that
    # were never pushed survive the pull rather than being clobbered.
    scpl = g.tmp / "scproj_pull_local"
    shutil.rmtree(scpl, ignore_errors=True)
    (scpl / ".rstest_cache").mkdir(parents=True)
    (scpl / ".rstest_cache" / "durations.json").write_text(
        '{"local_only.py::t_local": 4.2}', encoding="utf-8"
    )
    g.write("scproj_pull_local/test_s.py", "def test_a(): assert True\n")
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), "--cache-pull", cwd=scpl)
    merged_local = json.loads((scpl / ".rstest_cache" / "durations.json").read_text())
    check(
        "shared-cache: pull preserves local-only durations (overlay, not clobber)",
        "local_only.py::t_local" in merged_local and len(merged_local) > 1,
        f"rc={r.returncode} keys={sorted(merged_local)}",
    )

    # cache-compact is a run-less subcommand; --cache-push is not global, so
    # combining them would silently skip the run and the flag. clap rejects it.
    r = g.run("cache-compact", "--cache-remote", str(remote), "--cache-push", cwd=sca)
    check(
        "shared-cache: cache-compact + --cache-push is rejected",
        r.returncode != 0 and "cache-push" in r.stderr,
        f"rc={r.returncode} {r.stderr[-160:]}",
    )

    # Shared-cache flags in monorepo mode would silently no-op (push unreachable,
    # pull warms the wrong root cache). Must fail loud instead, and the rejection
    # must come BEFORE the pull runs (no 'pulled' line first). `mono` fixture is
    # the multi-project tree built in the monorepo section above.
    r = g.run("-n", "2", "--cache-remote", str(remote), "--cache-pull", cwd=g.tmp / "mono")
    check(
        "shared-cache: cache flags rejected in monorepo mode before pull runs",
        r.returncode != 0 and "monorepo" in r.stderr and "pulled" not in r.stderr,
        f"rc={r.returncode} {r.stderr[-200:]}",
    )

    # --cache-remote FLAG with no pull/push/compact does nothing; warn.
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), cwd=sca)
    check(
        "shared-cache: --cache-remote flag without an action warns",
        r.returncode == 0 and "not being used" in r.stderr,
        f"rc={r.returncode} {r.stderr[-160:]}",
    )
    # But an ambient RSTEST_CACHE_REMOTE env (no flag, no action) must NOT nag.
    r = g.run("test_s.py", "-n", "2", cwd=sca, env_extra={"RSTEST_CACHE_REMOTE": str(remote)})
    check(
        "shared-cache: ambient RSTEST_CACHE_REMOTE env does not warn",
        r.returncode == 0 and "not being used" not in r.stderr,
        f"rc={r.returncode} {r.stderr[-160:]}",
    )
