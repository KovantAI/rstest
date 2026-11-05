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
import contextlib
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import tomllib  # stdlib on 3.11+, this script's runtime

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
WORK = HERE / "work"
LOCK = HERE / "lock.json"
RESULTS = HERE / "results.json"
# One file per run (its suites only, plus what it ran against): results.json
# is the latest row per suite, this is the record over time.
HISTORY = HERE / "history"

PHASE_TIMEOUT = 2400  # seconds, per pytest/rstest run
# Baseline pytest is PINNED to the version rstest vendors: a newer pytest
# in the baseline venv produces version-skew collection diffs that look
# like rstest bugs (seen with packaging/jsonschema on pytest 9.1). rstest
# rstest vendors pytest 9.2.0, so the baseline matches it.
PYTEST_PIN = "pytest==9.2.0"
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


def _wipe(path):
    """rmtree that also clears mode-000 dirs some suites leave behind."""
    for root, dirs, _ in os.walk(path):  # top-down: chmod before descending
        for d in dirs:
            os.chmod(os.path.join(root, d), 0o700)
    shutil.rmtree(path, ignore_errors=True)


@contextlib.contextmanager
def _private_tmp(env):
    """A fresh TMPDIR for one runner invocation. Both runners otherwise share
    $TMPDIR/pytest-of-<user>, whose numbered-dir cleanup runs at every session
    end: a mode-000 dir one suite leaves there (flask's
    test_static_directory_without_*) makes pytest's rm_rf warn, and under a
    LATER suite's `filterwarnings = error` that crashes its session exit 1,
    pytest and rstest alike (2026-11-02: flask, click, jinja2). Made under
    /tmp, shorter than macOS's /var/folders/.../T: AF_UNIX socket paths cap at
    104 bytes, and anyio's socket tests broke under the longer root.
    """
    path = tempfile.mkdtemp(prefix="rsc-", dir="/tmp" if os.path.isdir("/tmp") else None)
    env["TMPDIR"] = path
    try:
        yield
    finally:
        _wipe(path)


def _run_meta(wheel, argv):
    def git(*a):
        return subprocess.run(["git", *a], cwd=REPO, capture_output=True, text=True).stdout.strip()

    return {
        "id": time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()),
        "rstest_commit": git("rev-parse", "--short=12", "HEAD"),
        "dirty": bool(git("status", "--porcelain", "--untracked-files=no")),
        "wheel": Path(wheel).name,
        "argv": argv,
    }


def _save(results, order, run, history=True):
    """Merge this run's rows into results.json (other suites keep their last
    row, so `--only` no longer wipes them) and write the run's history file.
    A row is replaced whole; `history=False` (prepare-only) merges its fields
    into the existing row instead, keeping that row's parity."""
    try:
        prior = {r["suite"]: r for r in json.loads(RESULTS.read_text())}
    except (FileNotFoundError, json.JSONDecodeError):
        prior = {}
    for name, row in results.items():
        prior[name] = {**row, "run": run["id"]} if history else {**prior.get(name, {}), **row}
    RESULTS.write_text(json.dumps([prior[n] for n in order if n in prior], indent=1))
    if history:
        HISTORY.mkdir(exist_ok=True)
        doc = {"run": run, "results": list(results.values())}
        (HISTORY / f"{run['id']}.json").write_text(json.dumps(doc, indent=1))


_EDITABLES = """
import json, importlib.metadata as m
out = []
for d in m.distributions():
    t = d.read_text("direct_url.json")
    if t and json.loads(t).get("dir_info", {}).get("editable"):
        out.append(json.loads(t)["url"])
print(json.dumps(out))
"""


def _editable_dirs(python, env=None):
    """Directories the venv has installed editable (PEP 610 direct_url.json)."""
    from urllib.parse import unquote, urlparse

    r = subprocess.run([str(python), "-c", _EDITABLES], capture_output=True, text=True, env=env)
    if r.returncode != 0:
        raise RuntimeError(f"editable probe failed: {r.stderr[-400:]}")
    return {Path(unquote(urlparse(u).path)).resolve() for u in json.loads(r.stdout)}


def _strip_opt(args, opt):
    """Drop an `--opt value` pair (e.g. `-n 4`) from an args list."""
    out, skip = [], False
    for a in args:
        if skip:
            skip = False
            continue
        if a == opt:
            skip = True
            continue
        out.append(a)
    return out


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

    def plugin_versions(self):
        """The pytest plugins installed in this suite's venv (offline), as
        `[{name, version, requires_pytest}]`; see corpus/plugin_probe.py."""
        r = sh([str(self.venv / "bin" / "python"), str(HERE / "plugin_probe.py")])
        if r.returncode != 0:
            raise RuntimeError(f"plugin probe failed: {r.stderr[-400:]}")
        return json.loads(r.stdout)

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
        self.check_editables()
        # Optional compat shim: a rootdir conftest.py written into the checkout,
        # loaded before collection by BOTH runners (same src), so parity holds.
        # Used to re-provide a private pytest symbol a pinned suite still imports
        # but the vendored/baseline pytest has since removed (e.g. flask's
        # `_pytest.monkeypatch.notset`, dropped in 9.1).
        shim = self.cfg.get("root_conftest")
        if shim and self.mode != "pyargs":
            (self.src / "conftest.py").write_text(shim)
            log(f"  {self.name}: wrote compat conftest.py")
        log(f"  {self.name}: prepared")

    def expected_editables(self):
        """Checkouts the install steps put in editable. A later step can
        silently swap one for a PyPI release when the checkout's version
        doesn't satisfy a dependency's floor (python-dateutil: tagless shallow
        clone -> setuptools_scm 0.1.devN < freezegun's >=2.7), and the suite
        then tests PyPI code, not the pinned commit."""
        if self.mode == "pyargs":
            return []
        if self.mode == "mono":
            return [self.src / proj for proj in self.cfg["projects"]]
        steps = self.cfg.get("install")
        if steps is None:  # the fallback attempts all install `-e .`
            return [self.src]
        pairs = [
            (a, b) for step in steps for a, b in zip(step.split(), step.split()[1:], strict=False)
        ]
        # `-r file` pulls its own `-e .` lines (httpx's requirements.txt).
        for a, b in list(pairs):
            if a == "-r" and (self.src / b).is_file():
                lines = (self.src / b).read_text().splitlines()
                pairs += [tuple(ln.split()[:2]) for ln in lines if len(ln.split()) >= 2]
        editable = any(a == "-e" and b.split("[")[0] == "." for a, b in pairs)
        return [self.src] if editable else []

    def check_editables(self):
        installed = _editable_dirs(self.venv / "bin" / "python", self.env())
        lost = [d for d in self.expected_editables() if d.resolve() not in installed]
        if lost:
            raise RuntimeError(
                f"editable checkout replaced by a PyPI release: {', '.join(map(str, lost))}. "
                "A later install step's version floor rejected the checkout's "
                "version; pin it via env SETUPTOOLS_SCM_PRETEND_VERSION (see python-dateutil)"
            )

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

    def run_pytest(self, xdist_workers=None, xdist_args=()):
        """Baseline run. `xdist_workers` (bench only) runs it under pytest-xdist
        at that -n instead, into its own snapshot, so the serial baseline is
        never overwritten. `xdist_args` (e.g. `--dist worksteal`) ride along
        with `-n` only."""
        if self.mode == "mono":
            if xdist_workers is not None:
                raise RuntimeError("xdist series is not supported in mono mode")
            return self.run_pytest_mono()
        snap = self.dir / ("pytest.json" if xdist_workers is None else "xdist.json")
        env = self.env()
        env["PYTHONPATH"] = str(HERE)  # recorder plugin
        env["RSTEST_RECORD"] = str(snap)
        # Drop any prior snapshot: a failed run that writes nothing must NOT be
        # silently diffed against a stale file (it reads as bogus parity).
        snap.unlink(missing_ok=True)
        xdist = [] if xdist_workers is None else ["-n", str(xdist_workers), *xdist_args]
        label = "pytest" if xdist_workers is None else f"xdist -n {xdist_workers}"
        log(f"  {self.name}: {label} starting")
        t0 = time.monotonic()
        with _private_tmp(env):
            r = sh(
                [
                    str(self.venv / "bin" / "python"),
                    "-m",
                    "pytest",
                    "-p",
                    "recorder",
                    "-q",
                    *xdist,
                    *self.target_args(),
                ],
                cwd=self.cwd(),
                env=env,
            )
        wall = time.monotonic() - t0
        log(f"  {self.name}: {label} done in {wall:.1f}s (rc={r.returncode})")
        snap.with_suffix(".log").write_text((r.stdout or "") + (r.stderr or ""))
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
        exits = []
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
            # target_args() (the suite's `args`, e.g. a `-W` filter) must ride
            # the mono baseline too, or it diverges from the rstest candidate
            # (which always forwards them) and reads as a false parity drop.
            with _private_tmp(env):
                r = sh(
                    [py, "-m", "pytest", "-p", "recorder", "-q", *self.target_args()],
                    cwd=proj_dir,
                    env=env,
                )
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
            exits.append(data.get("meta", {}).get("exitstatus"))
        # One root rstest run vs N pytest runs: the worst exit stands for the
        # set, but 5 (no tests collected) only when every project had none.
        real = [e for e in exits if e not in (None, 5)]
        combined["meta"]["exitstatus"] = max(real) if real else (5 if 5 in exits else None)
        merged = self.dir / "pytest.json"
        merged.write_text(json.dumps(combined, sort_keys=True))
        return merged, total

    def rstest_argv(self, snap, workers=None):
        extra = list(self.cfg.get("rstest_args", []))
        # `workers` (bench worker-sweep) is authoritative: strip any `-n N` from
        # the per-suite rstest_args and pin the requested count. Default (None)
        # keeps the suite's own policy, falling back to rstest's own `-n auto`.
        if workers is not None:
            extra = _strip_opt(extra, "-n")
            extra += ["-n", str(workers)]
        return [
            str(self.rstest_bin),
            "--report-json",
            str(snap),
            # hang backstop: a stuck suite becomes failures, not a stall
            "--worker-timeout",
            "120",
            *extra,
            *self.target_args(),
        ]

    def run_rstest(self, workers=None):
        snap = self.dir / "rstest.json"
        # Drop any prior snapshot: rstest exits non-zero and writes nothing on a
        # fatal error (e.g. no usable interpreter). A leftover file would pass
        # the exists() check below and get diffed as bogus parity.
        snap.unlink(missing_ok=True)
        nlabel = f"-n {workers}" if workers is not None else "-n auto"
        log(f"  {self.name}: rstest starting ({nlabel}, --worker-timeout 120)")
        t0 = time.monotonic()
        env = self.env()
        with _private_tmp(env):
            r = sh(self.rstest_argv(snap, workers), cwd=self.cwd(), env=env)
        wall = time.monotonic() - t0
        log(f"  {self.name}: rstest done in {wall:.1f}s (rc={r.returncode})")
        # The snapshot keeps only collect-error paths; the terminal summary has
        # their tracebacks (a `<session: ...>` error is otherwise undiagnosable).
        snap.with_suffix(".log").write_text((r.stdout or "") + (r.stderr or ""))
        if not snap.exists():
            raise RuntimeError(
                f"rstest produced no snapshot (rc={r.returncode})\n"
                f"{(r.stdout or '')[-600:]}\n{(r.stderr or '')[-400:]}"
            )
        return snap, wall


_UNSTABLE = [
    (re.compile(r"0x[0-9a-fA-F]{6,}"), "<addr>"),
    (re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"), "<uuid>"),
    # Wall-clock params (marshmallow builds IDs from `datetime.now()`): the two
    # runs straddle a second/minute boundary.
    (re.compile(r"\b\d{2}:\d{2}:\d{2}\b"), "<time>"),
    (re.compile(r"\b\d{4}-\d{2}-\d{2}\b|\b\d{2}-\d{2}-\d{4}\b"), "<date>"),
]


def _norm(nodeid):
    """Normalize run-dependent parametrize IDs (memory addresses, uuid4()).

    These differ between ANY two pytest processes, not just pytest-vs-rstest;
    pairing them up avoids reporting false missing/extra entries.
    """
    for rx, repl in _UNSTABLE:
        nodeid = rx.sub(repl, nodeid)
    # Multi-line IDs are pretty-printed reprs (pytest escapes the newlines to a
    # literal `\n`); pydantic-core dumps its fields map in per-process hash
    # order, so compare the lines as a multiset.
    # ponytail: line-sort, not a repr parser; pairs only leftovers whose
    # outcomes also agree, so a rare false pairing can't hide a diff.
    if "\\n" in nodeid:
        nodeid = "\\n".join(sorted(nodeid.split("\\n")))
    return nodeid


def _noisy_failure(entries, markers):
    """Every failing side failed only with a `noisy_failures` marker in its
    text (e.g. PytestUnraisableExceptionWarning: a resource leaked by an
    EARLIER test, GC'd during this one; which test catches it is co-location).
    ponytail: substring match on the failure text; a real failure whose text
    also carries a marker is excluded too. Structured causes if that bites."""
    failing = [e for e in entries if "failed" in (e.get("setup"), e.get("call"), e.get("teardown"))]
    return bool(failing) and all(any(m in e.get("longrepr", "") for m in markers) for e in failing)


def diff(baseline_path, candidate_path, noisy=(), noisy_failures=()):
    """Per-test outcome diff. `noisy` is a suite's node-ID prefixes whose
    outcome depends on the machine (live network, GC timing, upstream order
    pollution) and moves between any two runs; they are left out of the score
    and only counted. `noisy_failures` does the same by cause: error-type
    markers in the failure text (see `_noisy_failure`), for per-test
    mismatches and for session errors alike."""
    # Read each snapshot ONCE: the bench's gate needs collect-error counts too,
    # so surface them here rather than re-parsing the same files downstream.
    da = json.loads(Path(baseline_path).read_text())
    db = json.loads(Path(candidate_path).read_text())
    noisy = tuple(noisy)
    a = {k: v for k, v in da["tests"].items() if not k.startswith(noisy)}
    b = {k: v for k, v in db["tests"].items() if not k.startswith(noisy)}
    noisy_excluded = len((set(da["tests"]) | set(db["tests"])) - set(a) - set(b))
    keys = ("setup", "call", "teardown", "wasxfail")
    only_a = sorted(set(a) - set(b))
    only_b = sorted(set(b) - set(a))
    mismatch = []
    noisy_failed = 0
    for nid in set(a) & set(b):
        pa = {k: v for k, v in a[nid].items() if k in keys}
        pb = {k: v for k, v in b[nid].items() if k in keys}
        if pa != pb:
            if noisy_failures and _noisy_failure((a[nid], b[nid]), noisy_failures):
                noisy_failed += 1
            else:
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
    agree = len(set(a) & set(b)) - noisy_failed - len(mismatch) + unstable_pairs
    denom = max(len(set(a) | set(b)) - unstable_pairs - noisy_failed, 1)
    reprs = db.get("collect_error_reprs", {})
    session = [c for c in db.get("collect_errors", []) if c.startswith("<session: ")]
    noisy_session = [c for c in session if any(m in reprs.get(c, "") for m in noisy_failures)]
    # Of the candidate-only ("extra") tests, how many come from a module the
    # BASELINE failed to collect? Those are explained: rstest rescued a module
    # the drifting baseline dropped. Any extra from a module the baseline DID
    # collect is unexplained — phantom over-collection, a real rstest fault. The
    # bench's superset carve-out keys off this so it can't waive that fault.
    base_ce_modules = {c.split("::", 1)[0] for c in da.get("collect_errors", [])}
    extra_unexplained = sum(1 for nid in only_b if nid.split("::", 1)[0] not in base_ce_modules)
    return {
        "baseline_tests": len(da["tests"]),
        "candidate_tests": len(db["tests"]),
        "missing": only_a[:20],
        "missing_count": len(only_a),
        "extra": only_b[:20],
        "extra_count": len(only_b),
        "mismatch": sorted(mismatch)[:20],
        "mismatch_count": len(mismatch),
        "unstable_id_pairs": unstable_pairs,
        "noisy_excluded": noisy_excluded,
        "noisy_failure_excluded": noisy_failed,
        "noisy_session_errors": len(noisy_session),
        # Per-test parity can't see a run-level failure: a session error that
        # escapes pytest.main exits 1 with every test passed.
        # An error that escaped rstest's whole pytest session. Exit codes miss
        # it when the suite already fails (httpx/pydantic/urllib3, 2026-11-02),
        # and the recorder has no pytest-side equivalent to pair it with.
        "session_errors": [c for c in session if c not in noisy_session],
        "pytest_exit": da.get("meta", {}).get("exitstatus"),
        "rstest_exit": db.get("meta", {}).get("exitstatus"),
        "baseline_collect_errors": len(da.get("collect_errors", [])),
        "candidate_collect_errors": len(db.get("collect_errors", [])),
        "extra_unexplained_count": extra_unexplained,
        "score": round(100.0 * agree / denom, 2),
    }


def _status(res):
    """`diff` (per-test outcomes differ) wins over `session-error` (rstest's
    session itself crashed) and `exit-diff` (same outcomes, different exit
    code). A snapshot without an exit code can't be judged."""
    if res["score"] != 100.0:
        return "diff"
    if res.get("session_errors"):
        return "session-error"
    pe, re_ = res.get("pytest_exit"), res.get("rstest_exit")
    if pe is not None and re_ is not None and pe != re_:
        return "exit-diff"
    return "ok"


def _record_plugins(suite, res):
    """Record the suite venv's plugin versions into its result row. Plugins
    install unpinned, so versions move between runs; the docs' plugin-version
    tables are refreshed from this (corpus/plugin_versions.py). A probe
    failure is logged and never costs the suite its run."""
    try:
        res["plugins"] = suite.plugin_versions()
    except Exception as e:
        log(f"  {suite.name}: plugin probe failed: {str(e)[:200]}")


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
    order = list(suites_cfg)
    run = _run_meta(wheel, sys.argv[1:])
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
                _record_plugins(suite, results[name])
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
            _save(results, order, run, history=False)
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
        if "plugins" not in res:  # --execute-only: PREPARE didn't record them
            _record_plugins(suite, res)
        try:
            base, base_wall = suite.run_pytest()
            res["pytest_wall"] = round(base_wall, 1)
            cand, cand_wall = suite.run_rstest()
            res["rstest_wall"] = round(cand_wall, 1)
            res.update(
                diff(base, cand, suite.cfg.get("noisy", ()), suite.cfg.get("noisy_failures", ()))
            )
            res["status"] = _status(res)
            log(
                f"  {name}: parity {res['score']}% ({res['baseline_tests']} tests), "
                f"exit pytest={res['pytest_exit']} rstest={res['rstest_exit']}"
            )
        except subprocess.TimeoutExpired as e:
            res.update(status="timeout", error=str(e.cmd[:3]))
            log(f"  {name}: EXECUTE TIMEOUT")
        except Exception as e:
            res.update(status="error", error=str(e)[:600])
            log(f"  {name}: EXECUTE FAILED: {str(e)[:200]}")
        _save(results, order, run)

    # ---------- report ----------
    rows = list(results.values())
    print("\n| suite | tests | parity | pytest | rstest | status |")
    print("|---|---|---|---|---|---|")
    for r in rows:
        status = str(r.get("status", "-"))
        if status == "exit-diff":
            status += f" (pytest {r['pytest_exit']}, rstest {r['rstest_exit']})"
        elif status == "session-error" and isinstance(errs := r.get("session_errors"), list):
            status += f" ({', '.join(map(str, errs))}, see work/{r['suite']}/rstest.log)"
        print(
            f"| {r['suite']} | {r.get('baseline_tests', '-')} | "
            f"{r.get('score', '-')}% | {r.get('pytest_wall', '-')}s | "
            f"{r.get('rstest_wall', '-')}s | {status} |"
        )
    ok = sum(1 for r in rows if r.get("status") == "ok")
    log(f"=== {ok}/{len(rows)} suites at 100% parity with matching exit codes ===")


if __name__ == "__main__":
    main()
