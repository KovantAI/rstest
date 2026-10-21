"""The per-worker resource fixtures under plain pytest (the pytest11 plugin
`rstest_resources`), run through pytester with `-p rstest_worker.resources`,
which is what the entry point does once rstest is installed. Under rstest
they are covered by the e2e gate."""

from __future__ import annotations

import os
from pathlib import Path

import rstest_worker

pytest_plugins = ["pytester"]

# The inner pytest runs in a temp dir, so point it at this checkout's package.
PACKAGE_ROOT = str(Path(rstest_worker.__file__).resolve().parent.parent)


def _importable(monkeypatch):
    path = os.environ.get("PYTHONPATH")
    monkeypatch.setenv("PYTHONPATH", f"{PACKAGE_ROOT}{os.pathsep}{path}" if path else PACKAGE_ROOT)


TESTS = """
import os
import socket
from pathlib import Path


def test_ports_are_free_and_unique(rstest_free_port, request):
    other = request.getfixturevalue("rstest_free_port")
    assert rstest_free_port == other  # one value per test
    with socket.socket() as s:
        s.bind(("127.0.0.1", rstest_free_port))  # free when handed out


def test_another_port(rstest_free_port):
    with open(os.environ["SEEN"], "a") as f:
        f.write(f"{rstest_free_port}\\n")


def test_third_port(rstest_free_port):
    with open(os.environ["SEEN"], "a") as f:
        f.write(f"{rstest_free_port}\\n")


def test_worker_name(rstest_worker_name):
    assert rstest_worker_name("MyApp") == os.environ["EXPECT_NAME"]


def test_home(rstest_home):
    assert Path.home() == rstest_home
    assert Path(os.path.expanduser("~/.config")) == rstest_home / ".config"
    assert os.environ["XDG_CACHE_HOME"] == str(rstest_home / ".cache")
    (rstest_home / ".config" / "app.toml").write_text("x")
"""


def test_fixtures_work_under_plain_pytest(pytester, monkeypatch, tmp_path):
    _importable(monkeypatch)
    seen = tmp_path / "seen"
    monkeypatch.setenv("SEEN", str(seen))
    monkeypatch.setenv("EXPECT_NAME", "myapp_master")
    pytester.makepyfile(TESTS)
    result = pytester.runpytest_subprocess("-p", "rstest_worker.resources")
    result.assert_outcomes(passed=5)
    ports = seen.read_text().split()
    assert len(ports) == 2 and ports[0] != ports[1]


def test_worker_name_uses_an_existing_worker_id_fixture(pytester, monkeypatch):
    # pytest-xdist (or rstest) provides `worker_id`; the name follows it.
    _importable(monkeypatch)
    monkeypatch.setenv("EXPECT_NAME", "myapp_gw5")
    pytester.makeconftest(
        "import pytest\n\n@pytest.fixture(scope='session')\ndef worker_id():\n    return 'gw5'\n"
    )
    pytester.makepyfile(
        "import os\n\ndef test_name(rstest_worker_name):\n"
        "    assert rstest_worker_name('MyApp') == os.environ['EXPECT_NAME']\n"
    )
    result = pytester.runpytest_subprocess("-p", "rstest_worker.resources")
    result.assert_outcomes(passed=1)
