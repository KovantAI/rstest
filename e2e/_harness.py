"""Shared e2e gate harness: the Gate driver, check(), venv setup, git
helpers, and the fixture-suite string constants. Imported by gates/*.py."""

import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


E2E = REPO / "e2e"


FIXTURES = E2E / "fixtures"


WINDOWS = os.name == "nt"


def fx(name: str) -> str:
    """Load a fixture suite from e2e/fixtures/ (real .py files, not inlined
    string blobs). The gate writes these into per-test tmp dirs via g.write."""
    return (FIXTURES / name).read_text(encoding="utf-8")


def clear_hook_log(base: Path):
    # Conftest _log writes per-process files (base.<pid>): cross-process
    # appends to a single file are not atomic on Windows and drop lines.
    for p in glob.glob(str(base) + ".*"):
        os.unlink(p)


def read_hook_log(base: Path) -> str:
    parts = [Path(p).read_text(encoding="utf-8") for p in glob.glob(str(base) + ".*")]
    return "".join(parts)


def read_e2e_rows(base: Path) -> list:
    # Workers each write base.<worker>; gather and parse all lines.
    rows = []
    for p in glob.glob(str(base) + ".*"):
        for line in Path(p).read_text(encoding="utf-8").splitlines():
            if line.strip():
                rows.append(json.loads(line))
    return rows


def clear_e2e_log(base: Path):
    for p in glob.glob(str(base) + ".*"):
        Path(p).unlink()


def venv_bin(venv_dir: Path, name: str) -> Path:
    # venv layout differs: POSIX uses bin/, Windows uses Scripts/ + .exe.
    if WINDOWS:
        return venv_dir / "Scripts" / (name + ".exe")
    return venv_dir / "bin" / name


PASS = 0


FAIL = []


def check(name, cond, detail=""):
    global PASS
    if cond:
        PASS += 1
        print(f"  ok    {name}")
    else:
        FAIL.append(name)
        print(f"  FAIL  {name}  {detail}")


class Gate:
    def __init__(self, binary: Path, venv_dir: Path):
        self.binary = binary
        self.venv = venv_dir
        self.tmp = Path(tempfile.mkdtemp(prefix="rstest-gate-"))

    def run(self, *args, cwd=None, env_extra=None, timeout=120):
        env = dict(
            os.environ,
            VIRTUAL_ENV=str(self.venv),
            RSTEST_WORKER_PATH=str(REPO / "python"),
        )
        env.pop("PYTEST_ADDOPTS", None)
        # Doctor runs auto-publish to the CI job summary (GitHub step
        # summary / Buildkite annotation); keep the gate's fixture-suite
        # reports off the real run page.
        env.pop("GITHUB_STEP_SUMMARY", None)
        env.pop("BUILDKITE", None)
        # Bare --changed auto-targets the PR/MR base when a CI exposes it;
        # the gate's fixture repos have no origin, so a real PR CI run would
        # break every --changed check. Tests opt in via env_extra.
        for k in (
            "GITHUB_BASE_REF",
            "CI_MERGE_REQUEST_DIFF_BASE_SHA",
            "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
            "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        ):
            env.pop(k, None)
        if env_extra:
            env.update(env_extra)
        return subprocess.run(
            [str(self.binary), *args],
            cwd=cwd or str(self.tmp),
            env=env,
            capture_output=True,
            text=True,
            # rstest emits UTF-8 glyphs (✓ ✗ ─); pin the decode to UTF-8 so
            # the locale encoding (cp1252 on Windows) does not mangle them
            # and make `"✓" in r.stdout` spuriously False.
            encoding="utf-8",
            timeout=timeout,
        )

    def write(self, relpath, content):
        p = self.tmp / relpath
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
        return p


def find_python() -> str:
    # Vendored core requires >=3.10 (pyproject requires-python).
    if sys.version_info >= (3, 10):  # noqa: UP036 — launcher may run under older python
        return sys.executable
    for name in ("python3.13", "python3.12", "python3.11", "python3.10"):
        p = shutil.which(name)
        if p:
            return p
    sys.exit("gate needs python >= 3.10 on PATH")


BASE_DEPS = [
    "msgpack",
    "pluggy>=1.5",
    "iniconfig",
    "packaging",
    "pygments",
    "coverage",
    "pytest-cov",
]


def make_venv(venv_dir: Path, extra_deps=None):
    if venv_bin(venv_dir, "python").exists():
        return
    py = find_python()
    print(f"creating gate venv at {venv_dir} (from {py})")
    subprocess.run([py, "-m", "venv", str(venv_dir)], check=True)
    subprocess.run(
        [str(venv_bin(venv_dir, "pip")), "install", "-q", *BASE_DEPS, *(extra_deps or [])],
        check=True,
    )


def parse_ndjson(text):
    """Parse stdout as newline-delimited JSON. Returns (all_lines_valid,
    [objects]). A single embedded raw newline would split an object and
    fail json.loads - exactly the regression this guards against."""
    objs, ok = [], True
    for ln in text.splitlines():
        if not ln.strip():
            continue
        try:
            objs.append(json.loads(ln))
        except Exception:
            ok = False
    return ok, objs


def git(cwd, *args):
    """Run a git subcommand in cwd, raising on failure."""
    subprocess.run(["git", *args], cwd=cwd, check=True)


def git_commit(cwd, msg="base"):
    """Commit staged changes with a fixed throwaway identity (no gate check
    inspects the author)."""
    git(cwd, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", msg)


def git_init_commit(cwd, msg="base"):
    """init + add -A + commit: the standard fixture-repo bootstrap."""
    git(cwd, "init", "-q")
    git(cwd, "add", "-A")
    git_commit(cwd, msg)


BASIC = fx("basic.py")


CRASH = fx("crash.py")


CRASHFLAKY = fx("crashflaky.py")


CRASHLOOP = fx("crashloop.py")


CRASHMANY = fx("crashmany.py")


EACH_CRASH = fx("each_crash.py")


DISCO = fx("disco.py")


DOCTEST_MOD = fx("doctest_mod.py")


DOCTOR = fx("doctor.py")


DURATIONS_FIXTURE = fx("durations_fixture.py")


FLAKY = fx("flaky.py")


HANG = fx("hang.py")


LAZY_CONFTEST = fx("lazy_conftest.py")


LAZY_SESSION_A = fx("lazy_session_a.py")


LAZY_SESSION_B = fx("lazy_session_b.py")


LF = fx("lf.py")


MARKS = fx("marks.py")


MAXFAIL = fx("maxfail.py")


MAXFAIL_MANY = fx("maxfail_many.py")


SERIAL_CRASH = fx("serial_crash.py")


STEAL = fx("steal.py")


STEAL_SMALL = fx("steal_small.py")


MP_SPAWN = fx("mp_spawn.py")


NODECRASH_CONFTEST = fx("nodecrash_conftest.py")


NODECRASH_TEST = fx("nodecrash_test.py")


NODEHOOKS_CONFTEST = fx("nodehooks_conftest.py")


NODEHOOKS_TEST = fx("nodehooks_test.py")


NODEONEARG_CONFTEST = fx("nodeonearg_conftest.py")


SCOPE_A = fx("scope_a.py")


SCOPE_B = fx("scope_b.py")


SCOPE_C = fx("scope_c.py")


SECTIONS = fx("sections.py")


SERIAL = fx("serial.py")


WARN = fx("warn.py")
