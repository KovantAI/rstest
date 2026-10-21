"""`rstest_postgres_url` against a real, throwaway Postgres cluster.

Skips without `initdb` / `pg_ctl` on PATH or without psycopg. Two pytest
sessions play two workers (`gw0`, `gw1`) and run concurrently, so both clone
the same template at once, the case Postgres guards with "being accessed by
other users"."""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
from pathlib import Path

import pytest

psycopg = pytest.importorskip("psycopg")
if not (shutil.which("initdb") and shutil.which("pg_ctl")):
    pytest.skip("needs initdb / pg_ctl on PATH", allow_module_level=True)

import rstest_worker  # noqa: E402

PACKAGE_ROOT = str(Path(rstest_worker.__file__).resolve().parent.parent)

INNER = """
import os

import psycopg


def test_own_database_cloned_from_the_template(rstest_postgres_url):
    assert rstest_postgres_url.startswith("postgresql+psycopg://")
    assert rstest_postgres_url.endswith("/" + os.environ["EXPECT_DB"])
    with psycopg.connect(rstest_postgres_url.replace("+psycopg", "")) as conn:
        assert conn.execute("SELECT x FROM items").fetchall() == [(1,)]
        conn.execute("INSERT INTO items VALUES (%s)", (int(os.environ["WRITE"]),))
"""


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def cluster(tmp_path_factory):
    root = tmp_path_factory.mktemp("pg")
    data = root / "data"
    port = _free_port()
    subprocess.run(
        ["initdb", "-D", str(data), "-A", "trust", "-U", "postgres"],
        check=True,
        capture_output=True,
    )
    # TCP only: a temp dir is too long a path for a Unix socket on macOS.
    opts = f"-p {port} -c listen_addresses=127.0.0.1 -c unix_socket_directories=''"
    subprocess.run(
        ["pg_ctl", "-D", str(data), "-o", opts, "-l", str(root / "log"), "-w", "start"],
        check=True,
        capture_output=True,
    )
    try:
        yield f"postgresql://postgres@127.0.0.1:{port}"
    finally:
        subprocess.run(["pg_ctl", "-D", str(data), "-m", "immediate", "stop"], capture_output=True)


def _admin(cluster: str):
    return psycopg.connect(f"{cluster}/postgres", autocommit=True)


def test_each_worker_gets_a_clone_and_it_is_dropped(cluster, tmp_path):
    with _admin(cluster) as conn:
        conn.execute("CREATE DATABASE app_test")
        # A leftover from a crashed run: must be replaced, not reused.
        conn.execute("CREATE DATABASE app_test_gw0")
    with psycopg.connect(f"{cluster}/app_test", autocommit=True) as conn:
        conn.execute("CREATE TABLE items (x int)")
        conn.execute("INSERT INTO items VALUES (1)")

    procs = []
    for i, worker in enumerate(("gw0", "gw1")):
        d = tmp_path / worker
        d.mkdir()
        (d / "test_db.py").write_text(INNER)
        (d / "conftest.py").write_text(
            "import pytest\n\n@pytest.fixture(scope='session')\n"
            f"def worker_id():\n    return {worker!r}\n"
        )
        env = {
            **os.environ,
            "PYTHONPATH": PACKAGE_ROOT,
            "RSTEST_POSTGRES_URL": cluster.replace("postgresql://", "postgresql+psycopg://")
            + "/app_test",
            "EXPECT_DB": f"app_test_{worker}",
            "WRITE": str(10 + i),
        }
        procs.append(
            subprocess.Popen(
                [
                    sys.executable,
                    "-m",
                    "pytest",
                    "-p",
                    "rstest_worker.resources",
                    "-q",
                    "-p",
                    "no:cacheprovider",
                ],
                cwd=d,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
        )
    for p in procs:
        out, _ = p.communicate(timeout=120)
        assert p.returncode == 0, out

    with _admin(cluster) as conn:
        names = {r[0] for r in conn.execute("SELECT datname FROM pg_database")}
    assert "app_test_gw0" not in names and "app_test_gw1" not in names
    # The workers' writes went to their clones, never the template.
    with psycopg.connect(f"{cluster}/app_test") as conn:
        assert conn.execute("SELECT x FROM items").fetchall() == [(1,)]


def test_without_a_url_the_fixture_says_what_to_set(tmp_path):
    (tmp_path / "test_x.py").write_text("def test_x(rstest_postgres_url):\n    pass\n")
    env = {k: v for k, v in os.environ.items() if k not in ("RSTEST_POSTGRES_URL", "DATABASE_URL")}
    env["PYTHONPATH"] = PACKAGE_ROOT
    out = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-p",
            "rstest_worker.resources",
            "-q",
            "-p",
            "no:cacheprovider",
        ],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
    )
    assert out.returncode == 1
    assert "set RSTEST_POSTGRES_URL (or DATABASE_URL)" in out.stdout
