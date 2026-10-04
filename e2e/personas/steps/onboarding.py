"""Steps for features/onboarding.feature: the first-time evaluator."""

import os
import subprocess
import sys

from _harness import find_python, venv_bin
from pytest_bdd import given, parsers, then, when

from steps.common import q

_SEP = ";" if sys.platform == "win32" else ":"


def _purelib(py):
    return subprocess.run(
        [str(py), "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def _bare_project_venv(world):
    """A .venv with no packages at all (no pip), like a project venv rstest
    was never installed into. A .git dir bounds rstest's .venv walk. Returns
    the venv's purelib."""
    (world.project / ".git").mkdir(exist_ok=True)
    venv = world.project / ".venv"
    subprocess.run([find_python(), "-m", "venv", "--without-pip", str(venv)], check=True)
    return _purelib(venv_bin(venv, "python"))


@then("try does not silently report the suite as drop-in ready")
def _try_not_silent(world):
    out = world.result.stdout
    assert not ("drop-in ready" in out and "already red" not in out), world.tail()


@given("a project .venv that has the project's deps but not rstest")
def _venv_without_rstest(world):
    world.write("deps/onb_dep.py", "VALUE = 1\n")
    world.write(
        "tests/test_dep.py", "import onb_dep\n\ndef test_dep():\n    assert onb_dep.VALUE == 1\n"
    )
    purelib = _bare_project_venv(world)
    with open(f"{purelib}/onb_deps.pth", "w", encoding="utf-8") as f:
        f.write(str(world.project / "deps") + "\n")


@given("the worker interpreter is first on PATH")
def _worker_python_on_path(world):
    gate_bin = venv_bin(world.gate.venv, "python").parent
    world.notes["env"] = {"PATH": str(gate_bin) + _SEP + os.environ.get("PATH", "")}


@given("a usable project .venv")
def _usable_venv(world):
    world.write("tests/test_ok.py", "def test_ok():\n    pass\n")
    purelib = _bare_project_venv(world)
    # The worker venv's site-packages carry msgpack + pytest: the shim imports.
    with open(f"{purelib}/onb_gate.pth", "w", encoding="utf-8") as f:
        f.write(_purelib(venv_bin(world.gate.venv, "python")) + "\n")


@given("a .python-version pinning a different minor than the .venv")
def _stale_pin(world):
    minor = sys.version_info.minor
    world.write(".python-version", (f"3.{minor - 1}" if minor > 10 else f"3.{minor + 1}") + "\n")


@when(parsers.re(rf"I run {q('command')} with VIRTUAL_ENV unset"))
def _run_without_venv(world, command):
    world.run(command, env_extra=world.notes.get("env"), env_drop=("VIRTUAL_ENV",))


@then("the output mentions the missing dependency or that rstest is not installed in the .venv")
def _names_missing(world):
    out = world.output
    assert "onb_dep" in out or "rstest is not installed in it" in out, world.tail()


@then("the run succeeds, or the project .venv was rejected only for the version pin")
def _rejected_for_pin(world):
    out = world.output.replace("\\", "/")
    assert world.result.returncode == 0 or ("/.venv/" in out and "does not satisfy" in out), (
        world.tail()
    )


@then(parsers.re(rf"the run succeeds, or the output contains {q('text')}"))
def _succeeds_or_contains(world, text):
    assert world.result.returncode == 0 or text in world.output, world.tail()
