"""Warm-template workers (`--fork-pool=app`): the manifest filter, the guarded
template imports and the forked-child fixups."""

from __future__ import annotations

import json
import os
import sys
import textwrap
import threading
from types import ModuleType, SimpleNamespace

import pytest
from rstest_worker._internal import warm

# ── record: the manifest filter ─────────────────────────────────────────────


def _mod(name, path):
    m = ModuleType(name)
    if path is not None:
        m.__file__ = path
    return m


def test_filter_keeps_app_and_third_party_modules_in_import_order():
    root = "/proj"
    modules = {
        "django": _mod("django", "/venv/django/__init__.py"),
        "builtin_thing": _mod("builtin_thing", None),  # no __file__
        "pytest": _mod("pytest", "/v/pytest/__init__.py"),
        "_pytest.fixtures": _mod("_pytest.fixtures", "/v/_pytest/fixtures.py"),
        "rstest_worker._internal.stream": _mod("x", "/r/stream.py"),
        "myapp.settings": _mod("myapp.settings", "/proj/myapp/settings.py"),
        "tests.test_views": _mod("tests.test_views", "/proj/tests/test_views.py"),
        "conftest": _mod("conftest", "/proj/conftest.py"),
        "pytest_django.plugin": _mod("pytest_django.plugin", "/venv/pytest_django/plugin.py"),
        "helpers_test": _mod("helpers_test", "/proj/helpers_test.py"),  # python_files match
        "myhelpers.asserts": _mod("myhelpers.asserts", "/proj/myhelpers/asserts.py"),
        "numpy": _mod("numpy", "/venv/numpy/__init__.py"),
    }
    out = warm.filter_modules(
        modules,
        skip={"tests.test_views", "conftest"},
        rewrite={"pytest_django", "myhelpers"},
        pats=["test_*.py", "*_test.py"],
        rootdir=root,
    )
    assert out == ["django", "myapp.settings", "numpy"]


def test_record_takes_test_modules_conftests_and_rewrite_marks_from_the_session(monkeypatch):
    test_mod = _mod("t_mod", "/elsewhere/check_views.py")
    conftest = _mod("conftest", "/proj/conftest.py")
    hook = SimpleNamespace(_rewritten_names={"rewritten_one": None}, _must_rewrite={"plug"})
    from _pytest.assertion import assertstate_key

    config = SimpleNamespace(
        pluginmanager=SimpleNamespace(get_plugins=lambda: [conftest, object()]),
        stash={assertstate_key: SimpleNamespace(hook=hook)},
        getini=lambda name: ["check_*.py"],
        rootpath="/proj",
    )
    session = SimpleNamespace(config=config, items=[SimpleNamespace(module=test_mod)])
    fake = {
        "t_mod": test_mod,
        "conftest": conftest,
        "rewritten_one": _mod("rewritten_one", "/x/r.py"),
        "plug.sub": _mod("plug.sub", "/x/plug/sub.py"),
        "keep": _mod("keep", "/x/keep.py"),
    }
    monkeypatch.setattr(sys, "modules", fake)
    assert warm.record(session) == ["keep"]


# ── prewarm: guarded template imports ───────────────────────────────────────


@pytest.fixture
def app(tmp_path, monkeypatch):
    """Write modules into a fresh import dir; prewarm them; clean up after."""
    pkg = tmp_path / "pkgs"
    pkg.mkdir()
    created: list[str] = []
    cleanup: list = []

    def write(name, src):
        (pkg / f"{name}.py").write_text(textwrap.dedent(src))
        created.append(name)

    def run(modules, deny=None, rewrite=(), env=()):
        manifest = tmp_path / "manifest.json"
        manifest.write_text(
            json.dumps(
                {
                    "modules": modules,
                    "deny": deny or {},
                    "rewrite": list(rewrite),
                    "env": list(env),
                    "sys_path": [str(pkg)],
                }
            )
        )
        result = tmp_path / "result.json"
        clean = warm.prewarm(str(manifest), str(result))
        return clean, json.loads(result.read_text())

    yield SimpleNamespace(write=write, run=run, cleanup=cleanup)
    for fn in cleanup:
        fn()
    for name in created:
        sys.modules.pop(name, None)


def test_imports_the_manifest_and_restores_sys_path_and_environ(app):
    import gc

    app.write("wt_plain", "VALUE = 1\n")
    path, env = list(sys.path), os.environ
    gc.unfreeze()
    clean, result = app.run(["wt_plain"])
    # The template's objects are frozen so forked children share their pages.
    frozen = gc.get_freeze_count()
    gc.unfreeze()
    assert frozen > 0
    assert clean
    assert result["imported"] == 1 and result["denied"] == {} and result["hazard"] is None
    assert sys.modules["wt_plain"].VALUE == 1
    assert sys.path == path
    assert os.environ is env


@pytest.mark.parametrize(
    "read", ['os.environ["PYTEST_XDIST_WORKER"]', 'os.getenv("PYTEST_VERSION")']
)
def test_a_per_worker_env_read_is_a_hazard(app, read):
    app.write("wt_envread", f"import os\nWID = {read}\n")
    app.write("wt_after", "X = 1\n")
    clean, result = app.run(["wt_envread", "wt_after"])
    # Stopped before the read returned: the import fails, nothing leaked, so
    # the template carries on (no cold restart per env hazard).
    assert clean and result["hazard"] is None
    assert result["denied"]["wt_envread"].startswith("reads PYTEST_")
    assert "wt_envread" not in sys.modules
    assert "wt_after" in sys.modules


def test_an_env_write_at_import_is_a_hazard(app, monkeypatch):
    # Corpus regression (pretix): celery_app.py does
    # os.environ.setdefault("DJANGO_SETTINGS_MODULE", "pretix.settings"). Set in
    # the template, every worker inherited it before pytest-django read its ini.
    monkeypatch.delenv("WT_SETTINGS", raising=False)
    app.write("wt_envwrite", "import os\nos.environ.setdefault('WT_SETTINGS', 'prod')\n")
    clean, result = app.run(["wt_envwrite"])
    assert clean
    assert result["denied"]["wt_envwrite"] == "sets WT_SETTINGS at import"
    assert "WT_SETTINGS" not in os.environ


def test_a_read_of_a_key_the_session_changes_is_a_hazard(app, monkeypatch):
    # pretix.settings reads PRETIX_CONFIG_FILE, which the test settings set
    # first: read in the template, it would see it unset.
    monkeypatch.delenv("WT_CONFIG_FILE", raising=False)
    app.write("wt_cfgread", "import os\nCFG = os.environ.get('WT_CONFIG_FILE')\n")
    clean, result = app.run(["wt_cfgread"], env=["WT_CONFIG_FILE"])
    assert clean
    assert result["denied"]["wt_cfgread"] == "reads WT_CONFIG_FILE at import"


def test_an_env_hazard_in_a_dependency_denies_the_entry_too(app):
    # pretix: one run per module reading DJANGO_SETTINGS_MODULE; denying the
    # entry with its dependency lets the template converge in one run.
    app.write("wt_reader", "import os\nS = os.environ.get('RSTEST_WORKER_ID')\n")
    app.write("wt_entry", "import wt_reader\n")
    clean, result = app.run(["wt_entry"])
    assert clean
    assert result["denied"]["wt_reader"] == "reads RSTEST_WORKER_ID at import"
    assert "imports wt_reader" in result["denied"]["wt_entry"]


def test_a_swallowed_env_hazard_restarts_the_template(app):
    # A bare `except:` catches the guard's exception and carries on with a
    # fallback value the workers must not inherit.
    app.write(
        "wt_bare",
        "import os\ntry:\n    W = os.environ['RSTEST_WORKER_ID']\nexcept:\n    W = 'gw?'\n",
    )
    clean, result = app.run(["wt_bare"])
    assert not clean
    assert result["denied"]["wt_bare"].endswith("(and catches the guard's exception)")


def test_record_lists_the_env_keys_the_session_changed(monkeypatch):
    monkeypatch.setenv("WT_KEEP", "1")
    monkeypatch.setenv("WT_CHANGED", "before")
    monkeypatch.delenv("WT_ADDED", raising=False)
    monkeypatch.setattr(warm, "env_at_start", dict(os.environ))
    monkeypatch.setenv("WT_CHANGED", "after")
    monkeypatch.setenv("WT_ADDED", "x")
    config = SimpleNamespace(
        pluginmanager=SimpleNamespace(get_plugins=lambda: []),
        stash={},
        getini=lambda name: [],
        rootpath="/proj",
    )
    monkeypatch.setattr(sys, "modules", {})
    warm.record(SimpleNamespace(config=config, items=[]))
    assert warm.env_changed == ["WT_ADDED", "WT_CHANGED"]


def test_an_ordinary_env_read_is_fine_and_the_recorder_goes_inert(app):
    app.write("wt_home", "import os\nHOME = os.environ.get('HOME')\nENV = os.environ\n")
    clean, result = app.run(["wt_home"])
    assert clean and result["denied"] == {}
    # A module that kept a reference to the recorder still reads normally later.
    sys.modules["wt_home"].ENV.get("PYTEST_XDIST_WORKER")


def test_the_reader_is_named_not_the_module_that_imported_it(app):
    app.write("wt_cfg", "import os\nW = os.environ.get('RSTEST_WORKER_ID')\n")
    app.write("wt_outer", "import wt_cfg\n")
    _, result = app.run(["wt_outer"])
    assert result["denied"]["wt_cfg"] == "reads RSTEST_WORKER_ID at import"
    assert result["denied"]["wt_outer"].startswith("import error (imports wt_cfg")


def test_a_thread_started_at_import_is_a_hazard(app):
    stop = threading.Event()
    app.cleanup.append(stop.set)
    sys.modules["wt_stop"] = SimpleNamespace(event=stop)  # ty: ignore[invalid-assignment]
    app.cleanup.append(lambda: sys.modules.pop("wt_stop", None))
    app.write(
        "wt_thread",
        "import threading, wt_stop\n"
        "threading.Thread(target=wt_stop.event.wait, daemon=True).start()\n",
    )
    clean, result = app.run(["wt_thread"])
    assert not clean
    assert "thread" in result["denied"]["wt_thread"]


def test_a_socket_opened_at_import_is_a_hazard(app):
    app.write("wt_sock", "import socket\nS = socket.socket()\n")
    app.cleanup.append(
        lambda: sys.modules["wt_sock"].S.close() if "wt_sock" in sys.modules else None
    )
    clean, result = app.run(["wt_sock"])
    assert not clean
    assert result["denied"]["wt_sock"] == "opens a socket at import"


def test_a_module_keeping_a_stdio_stream_is_a_hazard(app):
    # Corpus regression (structlog): `from sys import stdout` in the template
    # binds the stream from before pytest's capture; 18 tests then failed under
    # =app and passed under =core.
    app.write("wt_stdio", "from sys import stdout\n")
    clean, result = app.run(["wt_stdio"])
    assert not clean
    assert result["denied"]["wt_stdio"].startswith("keeps a reference to sys.stdout")


def test_a_read_only_file_left_open_is_fine(app, tmp_path):
    data = tmp_path / "data.txt"
    data.write_text("x")
    app.write("wt_ro", f"F = open({str(data)!r})\n")
    app.cleanup.append(lambda: sys.modules["wt_ro"].F.close() if "wt_ro" in sys.modules else None)
    clean, result = app.run(["wt_ro"])
    assert clean and result["denied"] == {}


@pytest.mark.skipif(sys.platform != "darwin", reason="dyld images are macOS-only")
def test_a_system_framework_loaded_at_import_is_a_hazard(app):
    app.write(
        "wt_fw",
        "import ctypes\n"
        "ctypes.CDLL('/System/Library/Frameworks/AVFoundation.framework/AVFoundation')\n",
    )
    clean, result = app.run(["wt_fw"])
    assert not clean
    assert result["denied"]["wt_fw"].startswith("loads /System/Library/")


def test_an_import_error_is_denied_and_the_next_module_still_imports(app):
    app.write("wt_broken", "raise RuntimeError('settings not configured')\n")
    app.write("wt_ok", "X = 1\n")
    clean, result = app.run(["wt_broken", "wt_ok"])
    assert clean
    assert result["denied"]["wt_broken"].startswith("import error (RuntimeError")
    assert "wt_ok" in sys.modules and result["imported"] == 1


def test_a_module_importing_a_denied_one_is_denied_too(app):
    app.write("wt_bad", "X = 1\n")
    app.write("wt_user", "import wt_bad\n")
    clean, result = app.run(["wt_user"], deny={"wt_bad": "opens a socket at import"})
    assert clean
    assert "imports denied module wt_bad" in result["denied"]["wt_user"]
    assert "wt_bad" not in sys.modules


def test_a_module_importing_a_rewrite_marked_one_is_denied(app):
    # Corpus regression (packaging, urllib3, sqlalchemy, trio): an app module
    # pulled in hypothesis's `_hypothesis_globals`, which pytest marks for
    # rewrite; imported by the template first, pytest raised
    # PytestAssertRewriteWarning, a session error under filterwarnings=error.
    app.write("wt_plugpkg", "X = 1\n")
    app.write("wt_plugpkg_sub", "Y = 1\n")
    app.write("wt_lib", "import wt_plugpkg\n")
    clean, result = app.run(["wt_lib"], rewrite=["wt_plugpkg"])
    assert clean
    assert "wt_plugpkg, which pytest rewrites" in result["denied"]["wt_lib"]
    assert "wt_plugpkg" not in sys.modules and "wt_lib" not in sys.modules
    # A sibling whose name merely starts the same way is not caught.
    clean, result = app.run(["wt_plugpkg_sub"], rewrite=["wt_plugpkg"])
    assert result["imported"] == 1


def test_record_keeps_the_rewrite_marks(monkeypatch):
    from _pytest.assertion import assertstate_key

    hook = SimpleNamespace(_rewritten_names={}, _must_rewrite={"plug", "_plug_globals"})
    config = SimpleNamespace(
        pluginmanager=SimpleNamespace(get_plugins=lambda: []),
        stash={assertstate_key: SimpleNamespace(hook=hook)},
        getini=lambda name: [],
        rootpath="/proj",
    )
    monkeypatch.setattr(sys, "modules", {})
    warm.record(SimpleNamespace(config=config, items=[]))
    assert warm.rewrite_marked == ["_plug_globals", "plug"]


def test_a_module_that_swallows_a_template_import_failure_is_a_hazard(app):
    # Corpus regression (langchain): httpcore does `try: import anyio /
    # except ImportError: anyio = None`. The template denied anyio, httpcore
    # carried on without it, and every forked worker inherited anyio = None.
    app.write("wt_dep", "X = 1\n")
    app.write("wt_optional", "try:\n    import wt_dep\nexcept ImportError:\n    wt_dep = None\n")
    clean, result = app.run(["wt_optional"], deny={"wt_dep": "opens a socket at import"})
    assert not clean
    assert result["denied"]["wt_optional"] == (
        "imports wt_dep, which fails in the template, and carries on"
    )


def test_a_missing_optional_dependency_is_fine(app):
    app.write(
        "wt_soft",
        "try:\n    import wt_not_installed_anywhere\nexcept ImportError:\n    X = None\n",
    )
    clean, result = app.run(["wt_soft"])
    assert clean and result["denied"] == {} and result["imported"] == 1


def test_an_unreadable_manifest_is_skipped(tmp_path):
    (tmp_path / "m.json").write_text("{not json")
    assert warm.prewarm(str(tmp_path / "m.json"), str(tmp_path / "r.json"))
    assert "unreadable manifest" in json.loads((tmp_path / "r.json").read_text())["error"]


# ── forked child ─────────────────────────────────────────────────────────────


def test_reseed_after_fork_reseeds_numpy_and_torch(monkeypatch):
    calls = []
    monkeypatch.setitem(
        sys.modules, "numpy.random", SimpleNamespace(seed=lambda: calls.append("np"))
    )
    monkeypatch.setitem(sys.modules, "torch", SimpleNamespace(seed=lambda: calls.append("torch")))
    warm.reseed_after_fork()
    assert calls == ["np", "torch"]


def test_coverage_active_reads_the_running_coverage(monkeypatch):
    monkeypatch.setitem(
        sys.modules, "coverage", SimpleNamespace(Coverage=SimpleNamespace(current=lambda: object()))
    )
    assert warm.coverage_active()
    monkeypatch.setitem(
        sys.modules, "coverage", SimpleNamespace(Coverage=SimpleNamespace(current=lambda: None))
    )
    assert not warm.coverage_active()


def test_plugin_rewrite_names_covers_every_module_of_a_pytest11_dist(monkeypatch):
    import importlib.metadata

    ep = SimpleNamespace(group="pytest11", value="_hyp_plugin")
    plugin = SimpleNamespace(
        entry_points=[ep],
        files=["hyp/__init__.py", "_hyp_globals.py", "_hyp_plugin.py", "hyp-1.0.dist-info/RECORD"],
    )
    editable = SimpleNamespace(
        entry_points=[SimpleNamespace(group="pytest11", value="edit_pkg.plugin:Plugin")], files=[]
    )
    other = SimpleNamespace(entry_points=[], files=["numpy/__init__.py"])
    monkeypatch.setattr(importlib.metadata, "distributions", lambda: [plugin, editable, other])
    assert warm.plugin_rewrite_names() == {"hyp", "_hyp_globals", "_hyp_plugin", "edit_pkg"}
