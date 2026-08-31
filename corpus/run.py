#!/usr/bin/env python3
"""rstest compatibility corpus: run public pytest suites under pytest and
rstest, diff per-test outcomes, report.

Two strictly linear phases:

  PREPARE  — everything that touches the network: clone each repo (commit
             pinned via corpus/lock.json), create its venv, install its
             dependencies and the rstest wheel.
  EXECUTE  — fully offline: pytest baseline (recorder snapshot), rstest
             run (--report-json), per-test outcome diff.

Reproduce:
    python3 corpus/run.py                     # prepare, then execute
    python3 corpus/run.py --prepare-only      # prefetch (network)
    python3 corpus/run.py --execute-only      # offline rerun
    python3 corpus/run.py --only flask,click
    python3 corpus/run.py --wheel target/wheels/rstest-*.whl

Every step is traced with a timestamp. A broken suite records its error
and the run continues. Results: corpus/results.json + printed table.

Requires: git, uv, python >= 3.11 (tomllib).
"""

import argparse
import glob
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

import tomllib  # stdlib on 3.11+, this script's runtime

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
WORK = HERE / "work"
LOCK = HERE / "lock.json"
RESULTS = HERE / "results.json"

PHASE_TIMEOUT = 2400  # seconds, per pytest/rstest run
# Baseline pytest is PINNED to the version rstest vendors: a newer pytest
# in the baseline venv produces version-skew collection diffs that look
# like rstest bugs (seen with packaging/jsonschema on pytest 9.1).
PYTEST_PIN = "pytest==9.0.3"
NET_TIMEOUT = 900  # seconds, per clone / install step

T0 = time.monotonic()


def log(msg):
    print(f"[{time.monotonic() - T0:7.1f}s] {msg}", flush=True)


def sh(args, cwd=None, env=None, timeout=PHASE_TIMEOUT):
    return subprocess.run(
        args,
        cwd=cwd,
        env=env,
        timeout=timeout,
        capture_output=True,
        text=True,
    )


def load_lock():
    return json.loads(LOCK.read_text()) if LOCK.exists() else {}


def save_lock(lock):
    LOCK.write_text(json.dumps(lock, indent=1, sort_keys=True))


class Suite:
    def __init__(self, name, cfg, wheel, rstest_bin):
        self.name = name
        self.cfg = cfg
        self.wheel = wheel
        self.rstest_bin = rstest_bin
        self.dir = WORK / name
        self.src = self.dir / "src"
        self.venv = self.dir / "venv"
        self.mode = cfg.get("mode", "git")

    # -- environment -----------------------------------------------------
    def env(self):
        env = dict(os.environ, VIRTUAL_ENV=str(self.venv))
        env.pop("PYTEST_ADDOPTS", None)
        env["PATH"] = f"{self.venv}/bin:{env['PATH']}"
        # Stable set/dict iteration → stable parametrize IDs across the
        # baseline and candidate processes (pydantic et al. derive IDs
        # from set reprs).
        env["PYTHONHASHSEED"] = "0"
        env.update(self.cfg.get("env", {}))
        return env

    def cwd(self):
        # pyargs runs from the suite dir; git and mono run from the checkout root.
        return self.dir if self.mode == "pyargs" else self.src

    def pip(self, *specs, cwd=None):
        log(f"  {self.name}: install {' '.join(specs)}")
        r = sh(
            ["uv", "pip", "install", "-p", str(self.venv), *specs],
            cwd=cwd or self.cwd(),
            env=self.env(),
            timeout=NET_TIMEOUT,
        )
        if r.returncode != 0:
            raise RuntimeError(f"install failed: {specs}\n{r.stderr[-800:]}")

    # -- PREPARE (network) ------------------------------------------------
    def fetch(self, lock):
        self.dir.mkdir(parents=True, exist_ok=True)
        if self.mode == "pyargs":
            log(f"  {self.name}: no clone (pyargs mode)")
            return None
        if self.src.exists():
            sha = lock.get(self.name, "?")
            log(f"  {self.name}: already cloned ({str(sha)[:12]})")
            return lock.get(self.name)
        url = self.cfg["repo"]
        pinned = lock.get(self.name)
        log(f"  {self.name}: cloning {url}" + (f" @ {pinned[:12]}" if pinned else " (HEAD)"))
        args = ["git", "clone", "--depth", "1"]
        if self.cfg.get("submodules"):
            args.append("--recurse-submodules")
        r = sh([*args, url, str(self.src)], timeout=NET_TIMEOUT)
        if r.returncode != 0:
            raise RuntimeError(f"clone failed: {r.stderr[-400:]}")
        if pinned:
            r = sh(
                ["git", "fetch", "--depth", "1", "origin", pinned],
                cwd=self.src,
                timeout=NET_TIMEOUT,
            )
            if r.returncode == 0:
                sh(["git", "checkout", pinned], cwd=self.src)
                if self.cfg.get("submodules"):
                    sh(
                        ["git", "submodule", "update", "--init", "--depth", "1"],
                        cwd=self.src,
                        timeout=NET_TIMEOUT,
                    )
        sha = sh(["git", "rev-parse", "HEAD"], cwd=self.src).stdout.strip()
        log(f"  {self.name}: at {sha[:12]}")
        return sha

    def install(self):
        if (self.venv / "bin" / "python").exists():
            log(f"  {self.name}: venv exists")
        else:
            log(f"  {self.name}: creating venv")
            r = sh(["uv", "venv", "--python", sys.executable, str(self.venv)])
            if r.returncode != 0:
                raise RuntimeError(f"venv failed: {r.stderr[-400:]}")
        if self.mode == "pyargs":
            self.pip(*self.cfg["install"])
        elif self.mode == "mono":
            # Each lib is its own project (own pyproject + [dependency-groups]
            # test). Install all of them editable into the one shared venv, in
            # the listed order — cross-lib siblings (langgraph -> checkpoint,
            # prebuilt -> langgraph, ...) resolve to the local editable copies
            # because a later editable install of a package wins over the
            # transitive PyPI pull from an earlier one.
            for proj in self.cfg["projects"]:
                self.pip("--group", "test", "-e", ".", cwd=self.src / proj)
            self.apply_mono_policy()
            self.write_mono_projects()
        else:
            install = self.cfg.get("install")
            if install:
                for step in install:
                    self.pip(*step.split())
            else:
                last = None
                attempts = [
                    ["-e", ".[test]"],
                    ["-e", ".[tests]"],
                    ["-e", ".[testing]"],
                    ["-e", ".[dev]"],
                    ["--group", "tests", "-e", "."],
                    ["--group", "test", "-e", "."],
                    ["--group", "dev", "-e", "."],
                    ["-e", "."],
                ]
                for attempt in attempts:
                    try:
                        self.pip(*attempt)
                        last = None
                        break
                    except RuntimeError as e:
                        last = e
                if last:
                    raise last
        self.pip(PYTEST_PIN, str(self.wheel))
        log(f"  {self.name}: prepared")

    def apply_mono_policy(self):
        # Per-project policy, applied to the cloned pyprojects so BOTH the
        # baseline (per-lib pytest) and the candidate (one rstest root run)
        # honor it identically — parity is preserved by construction.
        #   serial:          libs pinned to -n 0 via their own [tool.rstest]
        #                     (e.g. a lib whose pytest-retry plugin needs
        #                     xdist's master-injected workerinput to run
        #                     parallel — single-worker sidesteps it and keeps
        #                     the `flaky` marker registered under strict-markers)
        #   disable_plugins:  -p no:NAME appended to the OTHER libs' addopts
        #                     (the plugin is in the shared venv and would load
        #                     — and crash under emulated xdist — everywhere)
        serial = set(self.cfg.get("serial", []))
        disable = self.cfg.get("disable_plugins", [])
        for proj in self.cfg["projects"]:
            pyproj = self.src / proj / "pyproject.toml"
            text = pyproj.read_text()
            if proj in serial:
                text += "\n[tool.rstest]\nnumprocesses = 0\n"
            elif disable:
                flags = " ".join(f"-p no:{name}" for name in disable)
                text = re.sub(
                    r'addopts\s*=\s*"([^"]*)"',
                    lambda m, flags=flags: f'addopts = "{m.group(1)} {flags}"',
                    text,
                    count=1,
                )
            pyproj.write_text(text)
            log(f"  {self.name}: policy applied to {proj}")

    # -- EXECUTE (offline) --------------------------------------------------
    def target_args(self):
        args = list(self.cfg.get("args", []))
        if self.mode == "pyargs":
            return ["--pyargs", self.cfg["package"], *args]
        return args

    def run_pytest(self):
        if self.mode == "mono":
            return self.run_pytest_mono()
        snap = self.dir / "pytest.json"
        env = self.env()
        env["PYTHONPATH"] = str(HERE)  # recorder plugin
        env["RSTEST_RECORD"] = str(snap)
        # Drop any prior snapshot: a failed run that writes nothing must NOT be
        # silently diffed against a stale file (it reads as bogus parity).
        snap.unlink(missing_ok=True)
        log(f"  {self.name}: pytest baseline starting")
        t0 = time.monotonic()
        r = sh(
            [
                str(self.venv / "bin" / "python"),
                "-m",
                "pytest",
                "-p",
                "recorder",
                "-q",
                *self.target_args(),
            ],
            cwd=self.cwd(),
            env=env,
        )
        wall = time.monotonic() - t0
        log(f"  {self.name}: pytest done in {wall:.1f}s (rc={r.returncode})")
        if not snap.exists():
            raise RuntimeError(
                f"pytest produced no snapshot (rc={r.returncode})\n"
                f"{(r.stdout or '')[-600:]}\n{(r.stderr or '')[-400:]}"
            )
        return snap, wall

    def write_mono_projects(self):
        # Pin the measured subset so rstest's root run matches the baseline
        # exactly (discovery would otherwise also pick up the DB-backed libs).
        # The root has no pytest config of its own, so monorepo mode still
        # engages. Written in BOTH prepare and execute: --execute-only skips
        # install, so without this an edited `projects` list would leave a
        # stale on-disk pyproject and the rstest candidate would measure a
        # different set than the (in-memory) baseline loop.
        projects_toml = ", ".join(f'"{p}"' for p in self.cfg["projects"])
        (self.src / "pyproject.toml").write_text(f"[tool.rstest]\nprojects = [{projects_toml}]\n")

    def run_pytest_mono(self):
        # The native workflow: pytest has no monorepo mode, so the baseline is
        # one serial pytest invocation per project. Wall time is the SUM (what a
        # developer actually waits through); per-test outcomes are merged into a
        # single doc, each nodeid prefixed with its project path to match the
        # root-relative keys rstest emits for the merged run.
        self.write_mono_projects()  # keep on-disk projects in sync (execute-only safe)
        combined = {"meta": {}, "collect_errors": [], "tests": {}}
        total = 0.0
        py = str(self.venv / "bin" / "python")
        for proj in self.cfg["projects"]:
            proj_dir = self.src / proj
            snap = self.dir / f"pytest-{proj.replace('/', '_')}.json"
            env = self.env()
            env["PYTHONPATH"] = str(HERE)  # recorder plugin
            env["RSTEST_RECORD"] = str(snap)
            snap.unlink(missing_ok=True)  # never diff against a stale snapshot
            log(f"  {self.name}: pytest baseline starting ({proj})")
            t0 = time.monotonic()
            r = sh([py, "-m", "pytest", "-p", "recorder", "-q"], cwd=proj_dir, env=env)
            wall = time.monotonic() - t0
            total += wall
            log(f"  {self.name}: pytest {proj} done in {wall:.1f}s (rc={r.returncode})")
            if not snap.exists():
                raise RuntimeError(
                    f"pytest produced no snapshot for {proj} (rc={r.returncode})\n"
                    f"{(r.stdout or '')[-600:]}\n{(r.stderr or '')[-400:]}"
                )
            data = json.loads(snap.read_text())
            for nid, v in data["tests"].items():
                combined["tests"][f"{proj}/{nid}"] = v
            combined["collect_errors"] += [f"{proj}/{p}" for p in data["collect_errors"]]
        merged = self.dir / "pytest.json"
        merged.write_text(json.dumps(combined, sort_keys=True))
        return merged, total

    def run_rstest(self):
        snap = self.dir / "rstest.json"
        # Drop any prior snapshot: rstest exits non-zero and writes nothing on a
        # fatal error (e.g. no usable interpreter). A leftover file would pass
        # the exists() check below and get diffed as bogus parity.
        snap.unlink(missing_ok=True)
        log(f"  {self.name}: rstest starting (-n auto, --worker-timeout 120)")
        t0 = time.monotonic()
        extra = self.cfg.get("rstest_args", [])
        r = sh(
            [
                str(self.rstest_bin),
                "--report-json",
                str(snap),
                # hang backstop: a stuck suite becomes failures, not a stall
                "--worker-timeout",
                "120",
                *extra,
                *self.target_args(),
            ],
            cwd=self.cwd(),
            env=self.env(),
        )
        wall = time.monotonic() - t0
        log(f"  {self.name}: rstest done in {wall:.1f}s (rc={r.returncode})")
        if not snap.exists():
            raise RuntimeError(
                f"rstest produced no snapshot (rc={r.returncode})\n"
                f"{(r.stdout or '')[-600:]}\n{(r.stderr or '')[-400:]}"
            )
        return snap, wall


_UNSTABLE = [
    (re.compile(r"0x[0-9a-fA-F]{6,}"), "<addr>"),
    (re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"), "<uuid>"),
]


def _norm(nodeid):
    """Normalize run-dependent parametrize IDs (memory addresses, uuid4()).

    These differ between ANY two pytest processes, not just pytest-vs-rstest;
    pairing them up avoids reporting false missing/extra entries.
    """
    for rx, repl in _UNSTABLE:
        nodeid = rx.sub(repl, nodeid)
    return nodeid


def diff(baseline_path, candidate_path):
    a = json.loads(Path(baseline_path).read_text())["tests"]
    b = json.loads(Path(candidate_path).read_text())["tests"]
    keys = ("setup", "call", "teardown", "wasxfail")
    only_a = sorted(set(a) - set(b))
    only_b = sorted(set(b) - set(a))
    mismatch = []
    for nid in set(a) & set(b):
        pa = {k: v for k, v in a[nid].items() if k in keys}
        pb = {k: v for k, v in b[nid].items() if k in keys}
        if pa != pb:
            mismatch.append(nid)
    # Second pass: pair leftover missing/extra whose normalized IDs match
    # and whose outcomes agree — count those as agreement, not diff.
    norm_b = {}
    for nid in only_b:
        norm_b.setdefault(_norm(nid), []).append(nid)
    unstable_pairs = 0
    still_a = []
    for nid in only_a:
        candidates = norm_b.get(_norm(nid))
        if candidates:
            other = candidates.pop(0)
            pa = {k: v for k, v in a[nid].items() if k in keys}
            pb = {k: v for k, v in b[other].items() if k in keys}
            if pa == pb:
                unstable_pairs += 1
                continue
            mismatch.append(nid)
            continue
        still_a.append(nid)
    only_a = still_a
    only_b = [nid for c in norm_b.values() for nid in c]
    agree = len(set(a) & set(b)) - len(mismatch) + unstable_pairs
    denom = max(len(set(a) | set(b)) - unstable_pairs, 1)
    return {
        "baseline_tests": len(a),
        "candidate_tests": len(b),
        "missing": only_a[:20],
        "missing_count": len(only_a),
        "extra": only_b[:20],
        "extra_count": len(only_b),
        "mismatch": sorted(mismatch)[:20],
        "mismatch_count": len(mismatch),
        "unstable_id_pairs": unstable_pairs,
        "score": round(100.0 * agree / denom, 2),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="comma-separated suite names")
    ap.add_argument("--skip", help="comma-separated suite names")
    ap.add_argument("--wheel", default=None, help="rstest wheel (default: newest in target/wheels)")
    ap.add_argument("--rstest", default=str(REPO / "target" / "release" / "rstest"))
    ap.add_argument("--prepare-only", action="store_true", help="network phase only")
    ap.add_argument(
        "--execute-only", action="store_true", help="offline phase only (assumes prepared)"
    )
    args = ap.parse_args()

    wheel = args.wheel or max(
        glob.glob(str(REPO / "target" / "wheels" / "rstest-*.whl")), key=os.path.getmtime
    )
    suites_cfg = tomllib.loads((HERE / "suites.toml").read_text())
    if args.only:
        keep = set(args.only.split(","))
        suites_cfg = {k: v for k, v in suites_cfg.items() if k in keep}
    if args.skip:
        drop = set(args.skip.split(","))
        suites_cfg = {k: v for k, v in suites_cfg.items() if k not in drop}

    suites = {name: Suite(name, cfg, wheel, args.rstest) for name, cfg in suites_cfg.items()}
    lock = load_lock()
    results: dict[str, dict[str, object]] = {name: {"suite": name} for name in suites}

    # ---------- PHASE 1: PREPARE (all network) ----------
    if not args.execute_only:
        log(f"=== PREPARE: {len(suites)} suites (wheel: {Path(wheel).name}) ===")
        for i, (name, suite) in enumerate(suites.items(), 1):
            log(f"[{i}/{len(suites)}] prepare {name}")
            try:
                sha = suite.fetch(lock)
                if sha:
                    lock[name] = sha
                    results[name]["commit"] = sha[:12]
                suite.install()
                results[name]["prepared"] = True
            except subprocess.TimeoutExpired as e:
                results[name].update(status="prepare-timeout", error=str(e.cmd[:3]))
                log(f"  {name}: PREPARE TIMEOUT")
            except Exception as e:
                results[name].update(status="prepare-error", error=str(e)[:600])
                log(f"  {name}: PREPARE FAILED: {str(e)[:200]}")
            save_lock(lock)
        prepared = [n for n, r in results.items() if r.get("prepared")]
        log(f"=== PREPARE done: {len(prepared)}/{len(suites)} ready ===")
        if args.prepare_only:
            RESULTS.write_text(json.dumps(list(results.values()), indent=1))
            return

    # ---------- PHASE 2: EXECUTE (offline) ----------
    log(f"=== EXECUTE: {len(suites)} suites ===")
    for i, (name, suite) in enumerate(suites.items(), 1):
        res = results[name]
        if args.execute_only:
            res["prepared"] = (suite.venv / "bin" / "python").exists()
        if not res.get("prepared"):
            log(f"[{i}/{len(suites)}] {name}: skipped (not prepared)")
            res.setdefault("status", "not-prepared")
            continue
        log(f"[{i}/{len(suites)}] execute {name}")
        try:
            base, base_wall = suite.run_pytest()
            res["pytest_wall"] = round(base_wall, 1)
            cand, cand_wall = suite.run_rstest()
            res["rstest_wall"] = round(cand_wall, 1)
            res.update(diff(base, cand))
            res["status"] = "ok" if res["score"] == 100.0 else "diff"
            log(f"  {name}: parity {res['score']}% ({res['baseline_tests']} tests)")
        except subprocess.TimeoutExpired as e:
            res.update(status="timeout", error=str(e.cmd[:3]))
            log(f"  {name}: EXECUTE TIMEOUT")
        except Exception as e:
            res.update(status="error", error=str(e)[:600])
            log(f"  {name}: EXECUTE FAILED: {str(e)[:200]}")
        RESULTS.write_text(json.dumps(list(results.values()), indent=1))

    # ---------- report ----------
    rows = list(results.values())
    print("\n| suite | tests | parity | pytest | rstest | status |")
    print("|---|---|---|---|---|---|")
    for r in rows:
        print(
            f"| {r['suite']} | {r.get('baseline_tests', '-')} | "
            f"{r.get('score', '-')}% | {r.get('pytest_wall', '-')}s | "
            f"{r.get('rstest_wall', '-')}s | {r.get('status', '-')} |"
        )
    ok = sum(1 for r in rows if r.get("status") == "ok")
    log(f"=== {ok}/{len(rows)} suites at 100% parity ===")


if __name__ == "__main__":
    main()
