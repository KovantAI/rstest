"""pytest-bdd wiring for the persona specs.

Every scenario gets its own project directory and a Gate bound to it, so
scenarios are independent and can run in any order. The built binary and the
worker venv are shared for the whole session.
"""

import sys
from pathlib import Path

import pytest
from pytest_bdd.steps import step_function_context_registry

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from _harness import REPO, WINDOWS, Gate, make_venv

pytest_plugins = [
    "steps.common",
    "steps.onboarding",
    "steps.migration",
    "steps.local_dev",
    "steps.ci",
    "steps.maintainer",
]

# Feature-file tags that gate a scenario on the platform: tag -> (skip?, reason).
PLATFORM_TAGS = {
    "posix_only": (WINDOWS, "POSIX-only behaviour"),
    "windows_only": (not WINDOWS, "Windows-only behaviour"),
    "linux_only": (sys.platform != "linux", "Linux-only behaviour"),
    "macos_only": (sys.platform != "darwin", "macOS-only behaviour"),
}


def pytest_addoption(parser):
    default_binary = REPO / "target" / "release" / ("rstest.exe" if WINDOWS else "rstest")
    parser.addoption("--binary", default=str(default_binary), help="rstest binary under test")
    parser.addoption(
        "--venv", default=str(REPO / ".gate-venv"), help="worker venv (created if missing)"
    )


def pytest_sessionstart(session):
    """Step text is global across steps/*.py and a later definition silently
    replaces an earlier one, so two personas writing the same phrase would
    change each other's scenarios. Refuse duplicates outright."""
    seen = {}
    for ctx in step_function_context_registry.values():
        key = (ctx.type, ctx.parser.name)
        func = ctx.step_func
        where = f"{func.__module__}.{func.__qualname__}"
        if seen.setdefault(key, where) != where:
            raise pytest.UsageError(
                f"step {ctx.type} {ctx.parser.name!r} is defined twice: {seen[key]} and {where}"
            )


def pytest_bdd_apply_tag(tag, function):
    if tag == "known_bug":
        mark = pytest.mark.xfail(reason="known bug: drop @known_bug once it is fixed", strict=True)
    elif tag in PLATFORM_TAGS:
        skip, reason = PLATFORM_TAGS[tag]
        mark = pytest.mark.skipif(skip, reason=reason)
    else:
        return None
    mark(function)
    return True


@pytest.fixture(scope="session")
def binary(pytestconfig):
    path = Path(pytestconfig.getoption("--binary")).resolve()
    if not path.exists():
        pytest.exit(f"binary missing: {path} (cargo build --release first)", returncode=4)
    return path


@pytest.fixture(scope="session")
def venv(pytestconfig):
    path = Path(pytestconfig.getoption("--venv")).resolve()
    make_venv(path)
    return path


@pytest.fixture
def gate(binary, venv, tmp_path):
    """A Gate whose scratch area is this scenario's own tmp dir."""
    return Gate(binary, venv, tmp=tmp_path)
