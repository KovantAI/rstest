"""Dead-master-path detector: classifier, recommendations, curated list and
gating (`--warn-on-dead-master-path`)."""

from __future__ import annotations

import re
import sys
import textwrap
from pathlib import Path
from types import SimpleNamespace

import pytest
from rstest_worker._internal import deadmaster
from rstest_worker._internal.deadmaster import CURATED, recommend, unit_findings


def find(src: str):
    return unit_findings([textwrap.dedent(src)])


def kinds(src: str):
    return [(k, key) for k, _line, key in find(src)]


# ── classifier ──────────────────────────────────────────────────────────────


def test_pytest_sugar_shape_is_not_flagged():
    # hasplugin("xdist") gate + defensive getattr read, terminal-only branch.
    assert not find(
        """
        def pytest_configure(config):
            if config.pluginmanager.hasplugin("xdist"):
                config.pluginmanager.register(DeferredXdistPlugin())
            if IS_SUGAR_ENABLED and not getattr(config, "slaveinput", None):
                standard = config.pluginmanager.getplugin("terminalreporter")
                config.pluginmanager.unregister(standard)
                config.pluginmanager.register(SugarTerminalReporter(), "terminalreporter")
        """
    )


def test_sqlalchemy_conftest_shape_is_not_flagged():
    # A real subscript, but the same conftest provisions the key in its own
    # pytest_configure_node, which rstest calls per worker.
    assert not find(
        """
        class XDistHooks:
            def pytest_configure_node(self, node):
                node.workerinput["follower_ident"] = "test_%s" % uuid4().hex[:12]

        def pytest_configure(config):
            if hasattr(config, "workerinput"):
                ident = config.workerinput["follower_ident"]
        """
    )


def test_provisioned_key_subscript_is_not_flagged():
    assert not find('seed = config.workerinput["randomly_seed"]')


def test_get_read_is_not_flagged():
    assert not find('seed = config.workerinput.get("my_seed", 0)')


def test_in_checked_key_is_not_flagged():
    assert not find(
        """
        if "my_seed" in config.workerinput:
            seed = config.workerinput["my_seed"]
        """
    )


def test_non_literal_key_is_not_flagged():
    assert not find("value = config.workerinput[name]")


def test_unprovisioned_subscript_is_a_crash_naming_the_key():
    assert kinds('port = config.workerinput["my_port"]') == [("crash", "my_port")]


def test_slaveinput_and_getattr_and_alias_subscripts_are_crashes():
    found = kinds(
        """
        a = config.slaveinput["a"]
        b = getattr(config, "workerinput")["b"]
        wi = getattr(config, "workerinput", None)
        c = wi["c"]
        """
    )
    assert sorted(found) == [("crash", "a"), ("crash", "b"), ("crash", "c")]


def test_pytest_html_shape_is_silent():
    found = kinds(
        """
        def pytest_configure(config):
            if not hasattr(config, "workerinput"):
                html = Report(path, config)
                config.pluginmanager.register(html)
        """
    )
    assert found == [("silent", None)]


@pytest.mark.parametrize(
    "gate",
    [
        'getattr(config, "workerinput", None) is None',
        "xdist.is_xdist_controller(config)",
        '"PYTEST_XDIST_WORKER" not in os.environ',
        'enabled and not hasattr(config, "slaveinput")',
    ],
)
def test_controller_gates_with_a_side_effect_are_silent(gate):
    assert kinds(f"if {gate}:\n    open(path, 'w')\n") == [("silent", None)]


def test_worker_gate_with_controller_else_or_early_return_is_silent():
    assert kinds(
        """
        if hasattr(config, "workerinput"):
            pass
        else:
            atexit.register(write_summary)
        """
    ) == [("silent", None)]
    assert kinds(
        """
        def pytest_configure(config):
            if hasattr(config, "workerinput"):
                return
            config.add_cleanup(write_summary)
        """
    ) == [("silent", None)]


def test_controller_gate_without_side_effect_is_not_flagged():
    assert not find('if not hasattr(config, "workerinput"):\n    tr.write_line("hello")\n')


def test_never_called_hook_is_silent():
    found = kinds(
        """
        class Plugin:
            def pytest_xdist_setupnodes(self, config, specs):
                pass
        """
    )
    assert found == [("silent", None)]


def test_syntax_error_is_skipped():
    assert unit_findings(["def broken(:\n", 'x = config.workerinput["k"]']) == [("crash", 1, "k")]


# ── recommendations ─────────────────────────────────────────────────────────


def test_crash_text_plugin_vs_conftest():
    found = [("crash", 7, "a"), ("crash", 9, "b"), ("crash", 11, "a")]
    plugin = recommend("pytest-foo 1.2", found, conftest=False)
    assert plugin.startswith(
        'rstest: warning: pytest-foo 1.2 reads workerinput["a"], workerinput["b"], '
        "which only an xdist controller sets"
    )
    assert "-p no:<name>" in plugin
    assert plugin.endswith("#plugins-that-expect-an-xdist-controller")
    conf = recommend("tests/conftest.py", found, conftest=True)
    assert conf.startswith('rstest: warning: tests/conftest.py:7 reads workerinput["a"]')
    assert "pytest_configure_node" in conf
    assert "-p no:" not in conf


def test_crash_outranks_silent():
    line = recommend("p 1", [("silent", 2, None), ("crash", 5, "k")], conftest=False)
    assert "KeyError" in line


def test_silent_text_plugin_vs_conftest():
    plugin = recommend("pytest-foo 1.2", [("silent", 3, None)], conftest=False)
    assert "pytest-foo 1.2 does its work only on the xdist controller" in plugin
    assert plugin.endswith("#the-silent-no-op-class")
    conf = recommend("conftest.py", [("silent", 3, None)], conftest=True)
    assert conf.startswith("rstest: warning: conftest.py:3 runs only when there is no workerinput")
    assert conf.endswith("#the-silent-no-op-class")


def test_curated_matches_the_top_100_matrix():
    doc = Path(__file__).parents[2] / "docs/reference/top-100-plugins.md"
    rows = re.findall(r"^\| \d+ \| ([^|]+?) \|", doc.read_text(encoding="utf-8"), re.M)
    assert len(rows) == 100
    assert set(rows) <= CURATED


# ── plugin and conftest units, gating ───────────────────────────────────────


class _Dist:
    def __init__(self, name, root: Path, files):
        self.metadata = {"Name": name}
        self.project_name = name
        self.version = "1.0"
        self.files = [_File(root, f) for f in files]


class _File:
    def __init__(self, root: Path, rel: str):
        self._root, self.parts, self.suffix = root, tuple(rel.split("/")), Path(rel).suffix

    def locate(self):
        return self._root / "/".join(self.parts)


class _PM:
    def __init__(self, distinfo, plugins=()):
        self._distinfo, self._plugins = distinfo, list(plugins)

    def list_plugin_distinfo(self):
        return self._distinfo

    def get_plugins(self):
        return self._plugins

    def get_name(self, plugin):
        return "foo"


_CRASH = 'port = config.workerinput["my_port"]\n'


def _setup(tmp_path, dist_name):
    pkg = tmp_path / "pytest_foo"
    (pkg / "sub").mkdir(parents=True, exist_ok=True)
    (pkg / "__init__.py").write_text("")
    (pkg / "sub" / "hooks.py").write_text(_CRASH)  # finding lives in a submodule
    (tmp_path / "conftest.py").write_text('if not hasattr(config, "workerinput"):\n    open(p)\n')
    plugin = SimpleNamespace(__name__="pytest_foo.plugin")
    dist = _Dist(dist_name, tmp_path, ["pytest_foo/__init__.py", "pytest_foo/sub/hooks.py"])
    conftest = SimpleNamespace(__file__=str(tmp_path / "conftest.py"))
    config = SimpleNamespace(pluginmanager=_PM([(plugin, dist)], [conftest]), rootpath=tmp_path)
    return config


@pytest.fixture
def scan(monkeypatch):
    monkeypatch.setattr(deadmaster, "_scanned", False)
    monkeypatch.setenv(deadmaster.ENV, "1")
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    return monkeypatch


def test_scan_reports_plugin_submodule_and_conftest(scan, tmp_path, capsys):
    deadmaster.warn_dead_master(_setup(tmp_path, "pytest-foo"))
    err = capsys.readouterr().err.splitlines()
    assert len(err) == 2
    assert err[0].startswith('rstest: warning: pytest-foo 1.0 reads workerinput["my_port"]')
    assert "-p no:foo" in err[0]
    assert err[1].startswith("rstest: warning: conftest.py:1 runs only when")


def test_curated_plugin_is_not_scanned(scan, tmp_path, capsys):
    deadmaster.warn_dead_master(_setup(tmp_path, "pytest_html"))
    err = capsys.readouterr().err
    assert "pytest_html" not in err
    assert "conftest.py" in err


def test_no_scan_without_env(scan, tmp_path, capsys):
    scan.delenv(deadmaster.ENV)
    deadmaster.warn_dead_master(_setup(tmp_path, "pytest-foo"))
    assert capsys.readouterr().err == ""


def test_no_scan_on_workers_other_than_gw0(scan, tmp_path, capsys):
    scan.setenv("RSTEST_WORKER_ID", "gw1")
    deadmaster.warn_dead_master(_setup(tmp_path, "pytest-foo"))
    assert capsys.readouterr().err == ""
    scan.setenv("RSTEST_WORKER_ID", "gw0")
    deadmaster.warn_dead_master(_setup(tmp_path, "pytest-foo"))
    assert "pytest-foo" in capsys.readouterr().err


def test_scanner_exception_never_breaks_the_run(scan, capsys):
    deadmaster.warn_dead_master(SimpleNamespace(pluginmanager=None, rootpath="."))
    assert capsys.readouterr().err == ""


def test_stream_configure_runs_the_scan_only_when_enabled(monkeypatch):
    # Not imported at all without the env var (import-cost budget).
    monkeypatch.delitem(sys.modules, "rstest_worker._internal.deadmaster", raising=False)
    from rstest_worker._internal import stream

    calls = []
    monkeypatch.setattr(deadmaster, "warn_dead_master", calls.append)
    plugin = stream.StreamPlugin(SimpleNamespace(send=lambda *a: None))
    config = SimpleNamespace(
        option=SimpleNamespace(), addinivalue_line=lambda *a: None, pluginmanager=_PM([])
    )
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    monkeypatch.delenv(deadmaster.ENV, raising=False)
    plugin.pytest_configure(config)
    assert calls == []
    monkeypatch.setitem(sys.modules, "rstest_worker._internal.deadmaster", deadmaster)
    monkeypatch.setenv(deadmaster.ENV, "1")
    plugin.pytest_configure(config)
    assert calls == [config]
