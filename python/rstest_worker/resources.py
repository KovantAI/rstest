"""Per-worker resources: the usual parallel-safety fixes as fixtures.

A plain pytest plugin, registered through the `pytest11` entry point
`rstest_resources`, so the fixtures work under rstest, under plain pytest and
under pytest-xdist alike while rstest is installed (a suite using them doesn't
become rstest-only). Disable with `-p no:rstest_resources`.

The worker identity comes from the `worker_id` fixture when one exists (rstest
and pytest-xdist both provide it: `gw0`, `gw1`, ...) and is `master` otherwise,
the xdist convention for a run without workers.
"""

from __future__ import annotations

import socket

import pytest

# Ports `rstest_free_port` handed out in this process (never repeated).
_given: set[int] = set()


def _worker_id(request: pytest.FixtureRequest) -> str:
    try:
        return str(request.getfixturevalue("worker_id"))
    except pytest.FixtureLookupError:
        return "master"


@pytest.fixture
def rstest_free_port() -> int:
    """A TCP port on 127.0.0.1 that was free when handed out, chosen by the OS
    (bind to port 0), never a fixed range that can collide with other
    services. A worker never hands out the same port twice in a session.
    Use it in place of a hard-coded port so parallel workers don't race for
    one."""
    while True:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        if port not in _given:
            _given.add(port)
            return port


@pytest.fixture(scope="session")
def rstest_worker_name(request: pytest.FixtureRequest):
    """`name(base)` -> `"<base>_<worker id>"` (`myapp_gw3`, or `myapp_master`
    without workers): one database, schema, Redis prefix or queue per worker.
    Lowercase letters, digits and underscores, a valid Postgres name."""
    worker = _worker_id(request)

    def name(base: str) -> str:
        return f"{base}_{worker}".lower()

    return name


@pytest.fixture
def rstest_home(tmp_path, monkeypatch):
    """A fresh home directory for this test: `HOME` (and `USERPROFILE`),
    `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_DATA_HOME` point inside it,
    so `~/.config` writes and `Path.home()` don't collide across workers or
    leak between tests. Restored after the test."""
    home = tmp_path / "home"
    for sub in (".config", ".cache", ".local/share"):
        (home / sub).mkdir(parents=True)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(home / ".config"))
    monkeypatch.setenv("XDG_CACHE_HOME", str(home / ".cache"))
    monkeypatch.setenv("XDG_DATA_HOME", str(home / ".local" / "share"))
    return home


# ---- Postgres: one database per worker, cloned from a migrated template ----


def _pg_connect(url: str):
    """An autocommit connection with psycopg 3, else psycopg2."""
    try:
        import psycopg  # ty: ignore[unresolved-import]

        return psycopg.connect(url, autocommit=True)
    except ImportError:
        pass
    try:
        import psycopg2  # ty: ignore[unresolved-import]
    except ImportError:
        pytest.fail(
            "rstest_postgres_url needs a Postgres driver: pip install psycopg (or psycopg2)",
            pytrace=False,
        )
    conn = psycopg2.connect(url)
    conn.autocommit = True
    return conn


def _quote_ident(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def _with_database(url: str, database: str) -> str:
    from urllib.parse import urlsplit, urlunsplit

    parts = urlsplit(url)
    return urlunsplit(parts._replace(path="/" + database))


def _libpq_url(url: str) -> str:
    """`postgresql+psycopg://...` (SQLAlchemy style) -> `postgresql://...`."""
    scheme, sep, rest = url.partition("://")
    return scheme.split("+", 1)[0] + sep + rest


def _pg_execute(conn, sql: str, params=None):
    cur = conn.cursor()
    try:
        cur.execute(sql, params)
        return cur.fetchall() if cur.description else None
    finally:
        cur.close()


def _drop(conn, name: str, force: bool) -> None:
    with_force = " WITH (FORCE)" if force else ""
    _pg_execute(conn, f"DROP DATABASE IF EXISTS {_quote_ident(name)}{with_force}")


@pytest.fixture(scope="session")
def rstest_postgres_url(request):
    """A Postgres database of this worker's own, as a URL.

    Takes the base URL from `RSTEST_POSTGRES_URL` (else `DATABASE_URL`), e.g.
    `postgresql://ci:ci@localhost:5432/app_test`, and creates `app_test_gw3`
    (`app_test_master` without workers). When the base database exists it is
    cloned as a TEMPLATE: migrate it once before the run and every worker
    starts with the schema in place. A leftover from a crashed run is dropped
    first, and the database is dropped when the session ends. The URL keeps
    the base URL's scheme (`postgresql+psycopg://` stays usable by
    SQLAlchemy). Needs psycopg (3) or psycopg2."""
    import os
    import time
    from urllib.parse import urlsplit

    base = os.environ.get("RSTEST_POSTGRES_URL") or os.environ.get("DATABASE_URL")
    if not base:
        pytest.fail(
            "rstest_postgres_url: set RSTEST_POSTGRES_URL (or DATABASE_URL) to the "
            "base database, e.g. postgresql://user:pass@localhost:5432/app_test",
            pytrace=False,
        )
    template = urlsplit(base).path.lstrip("/") or "postgres"
    name = f"{template}_{_worker_id(request)}".lower()
    admin = _pg_connect(_with_database(_libpq_url(base), "postgres"))
    try:
        version = int(_pg_execute(admin, "SHOW server_version_num")[0][0])
        force = version >= 130000
        _drop(admin, name, force)
        exists = _pg_execute(admin, "SELECT 1 FROM pg_database WHERE datname = %s", (template,))
        create = f"CREATE DATABASE {_quote_ident(name)}"
        if exists and template != "postgres":
            create += f" TEMPLATE {_quote_ident(template)}"
        # Postgres refuses to clone a template another session is using
        # (another worker's clone included): retry with backoff.
        for attempt in range(20):
            try:
                _pg_execute(admin, create)
                break
            except Exception as e:  # driver-specific error classes
                if "being accessed by other users" not in str(e) or attempt == 19:
                    raise
                time.sleep(0.05 * (attempt + 1))
    finally:
        admin.close()
    yield _with_database(base, name)
    admin = _pg_connect(_with_database(_libpq_url(base), "postgres"))
    try:
        _drop(admin, name, force)
    finally:
        admin.close()
