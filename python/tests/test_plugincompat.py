"""Unit tests for third-party-plugin neutralization helpers."""

import zlib

from rstest_worker._internal import plugincompat
from rstest_worker._internal.plugincompat import (
    _is_dist_internal,
    _neutralize_rerunfailures,
    _pytest_pin_conflicts,
    _random_order_seed,
    _randomly_seed,
    _warn_pytest_pins,
)


def test_randomly_seed_hex_uid():
    assert _randomly_seed("ff") == 0xFF
    # Masked to 32 bits.
    assert _randomly_seed("1" + "0" * 8) == (0x100000000 & 0xFFFFFFFF)


def test_randomly_seed_is_32_bit():
    for uid in ("deadbeefdeadbeef", "not-hex", ""):
        seed = _randomly_seed(uid)
        assert 0 <= seed <= 0xFFFFFFFF


def test_randomly_seed_non_hex_uses_crc32():
    assert _randomly_seed("not-hex") == zlib.crc32(b"not-hex") & 0xFFFFFFFF


def test_randomly_seed_deterministic():
    # Same uid -> same seed across calls (the cross-worker agreement contract).
    assert _randomly_seed("abc123") == _randomly_seed("abc123")
    assert _randomly_seed("zzz") == _randomly_seed("zzz")


def test_randomly_seed_non_string():
    # A non-str/non-hex value still yields a deterministic 32-bit seed.
    assert _randomly_seed(None) == zlib.crc32(b"None") & 0xFFFFFFFF


class _OptConfig:
    """Minimal config exposing getoption for the random-order seed helper."""

    def __init__(self, opt, raise_on_get=False):
        self._opt = opt
        self._raise = raise_on_get

    def getoption(self, name):
        if self._raise:
            raise ValueError("unknown option: " + name)
        return self._opt


def test_random_order_seed_derives_shared_default_from_uid():
    # Default (plugin's per-process "default:<rand>"): replace with a value
    # derived from the run uid so every worker agrees, keeping the "default:"
    # prefix so the plugin stays disabled unless the user opts in.
    seed = _random_order_seed(_OptConfig("default:987654"), "abc123")
    assert seed == "default:" + str(_randomly_seed("abc123"))
    assert seed.startswith("default:")
    # Same uid -> same seed (the cross-worker agreement contract).
    assert seed == _random_order_seed(_OptConfig("default:111"), "abc123")


def test_random_order_seed_honors_explicit_pin():
    # A user-pinned seed (no "default:" prefix) is passed through verbatim.
    assert _random_order_seed(_OptConfig("42"), "abc123") == "42"


def test_random_order_seed_when_plugin_absent_still_derives():
    # getoption raising (option not registered / plugin absent) -> derive a
    # harmless shared default; the key is unused when the plugin isn't loaded.
    seed = _random_order_seed(_OptConfig(None, raise_on_get=True), "ff")
    assert seed == "default:" + str(_randomly_seed("ff"))


class _Plugin:
    def __init__(self, name=None, module="somepkg.plugin"):
        if name is not None:
            self.__name__ = name
        _Plugin.__module__ = module


def test_is_dist_internal_by_name():
    assert _is_dist_internal(_Plugin(name="xdist.plugin"))
    assert _is_dist_internal(_Plugin(name="pytest_cov.plugin"))
    assert not _is_dist_internal(_Plugin(name="pytest_django.plugin"))


def test_is_dist_internal_by_module():
    class XdistObj:
        pass

    XdistObj.__module__ = "xdist.dsession"
    assert _is_dist_internal(XdistObj())

    class Other:
        pass

    Other.__module__ = "myplugin.core"
    assert not _is_dist_internal(Other())


class _FakePM:
    def __init__(self, plugin):
        self._plugin = plugin
        self.unregistered = []

    def get_plugin(self, name):
        return self._plugin if name == "rerunfailures" else None

    def unregister(self, plugin):
        self.unregistered.append(plugin)


class _FakeConfig:
    def __init__(self, plugin):
        self.pluginmanager = _FakePM(plugin)


def test_neutralize_rerunfailures_unregisters_when_present():
    sentinel = object()
    config = _FakeConfig(sentinel)
    _neutralize_rerunfailures(config)
    assert config.pluginmanager.unregistered == [sentinel]


def test_neutralize_rerunfailures_noop_when_absent():
    config = _FakeConfig(None)
    _neutralize_rerunfailures(config)
    assert config.pluginmanager.unregistered == []


class _RetryPM:
    """Plugin manager stub: `plugins` maps registered name -> plugin object."""

    def __init__(self, plugins):
        self._plugins = plugins
        self.unregistered = []

    def get_plugin(self, name):
        return self._plugins.get(name)

    def has_plugin(self, name):
        return name in self._plugins

    def unregister(self, plugin):
        self.unregistered.append(plugin)


class _RetryConfig:
    def __init__(self, plugins, workerinput):
        self.pluginmanager = _RetryPM(plugins)
        if workerinput is not None:
            self.workerinput = workerinput


def _install_fake_report_server(monkeypatch, port=54321):
    import sys
    import types

    calls = {"created": 0}

    class _FakeReportServer:
        def __init__(self):
            calls["created"] += 1

        def initialize_server(self):
            return port

    mod = types.ModuleType("pytest_retry.server")
    setattr(mod, "ReportServer", _FakeReportServer)  # noqa: B010
    pkg = types.ModuleType("pytest_retry")
    setattr(pkg, "server", mod)  # noqa: B010
    monkeypatch.setitem(sys.modules, "pytest_retry", pkg)
    monkeypatch.setitem(sys.modules, "pytest_retry.server", mod)
    return calls


def test_seed_pytest_retry_seeds_server_port(monkeypatch):
    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    calls = _install_fake_report_server(monkeypatch, port=45678)
    wi = {}
    config = _RetryConfig({"pytest-retry": object()}, wi)
    _seed_pytest_retry(config)
    assert wi["server_port"] == 45678
    assert calls["created"] == 1
    # the live server must be pinned to the config for the session's lifetime
    assert getattr(config, "_rstest_retry_server", None) is not None


def test_seed_pytest_retry_noop_without_plugin(monkeypatch):
    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    _install_fake_report_server(monkeypatch)
    wi = {}
    config = _RetryConfig({}, wi)  # no pytest-retry
    _seed_pytest_retry(config)
    assert "server_port" not in wi


def test_seed_pytest_retry_skips_when_xdist_present(monkeypatch):
    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    calls = _install_fake_report_server(monkeypatch)
    wi = {}
    # real xdist installed -> pytest-retry self-provisions via its master branch
    config = _RetryConfig({"pytest-retry": object(), "xdist": object()}, wi)
    _seed_pytest_retry(config)
    assert "server_port" not in wi
    assert calls["created"] == 0


def test_seed_pytest_retry_respects_underscore_name(monkeypatch):
    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    _install_fake_report_server(monkeypatch, port=9999)
    wi = {}
    config = _RetryConfig({"pytest_retry": object()}, wi)  # underscore variant
    _seed_pytest_retry(config)
    assert wi["server_port"] == 9999


def test_seed_pytest_retry_falls_back_to_neutralize_on_import_error(monkeypatch):
    import sys

    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    # No fake module installed -> import fails -> plugin is unregistered instead.
    monkeypatch.setitem(sys.modules, "pytest_retry", None)
    plugin = object()
    config = _RetryConfig({"pytest-retry": plugin}, {})
    _seed_pytest_retry(config)
    assert config.pluginmanager.unregistered == [plugin]


def test_seed_pytest_retry_noop_without_workerinput(monkeypatch):
    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    calls = _install_fake_report_server(monkeypatch)
    # no workerinput attr at all -> nothing a master would have staged -> skip
    config = _RetryConfig({"pytest-retry": object()}, None)
    _seed_pytest_retry(config)
    assert calls["created"] == 0
    assert getattr(config, "_rstest_retry_server", None) is None


def test_seed_pytest_retry_noop_when_port_already_set(monkeypatch):
    from rstest_worker._internal.plugincompat import _seed_pytest_retry

    calls = _install_fake_report_server(monkeypatch)
    wi = {"server_port": 111}  # already seeded -> don't orphan a second server
    config = _RetryConfig({"pytest-retry": object()}, wi)
    _seed_pytest_retry(config)
    assert wi["server_port"] == 111
    assert calls["created"] == 0


# --- pytest-mypy dead-master-path seeding (mypy_config_stash_serialized) ---
# Reuses _RetryPM/_RetryConfig (a get_plugin/has_plugin/unregister + workerinput
# stub); pytest-mypy registers under the entrypoint name "mypy".


def test_seed_pytest_mypy_seeds_stash_path():
    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    wi = {}
    config = _RetryConfig({"mypy": object()}, wi)
    _seed_pytest_mypy(config)
    # A unique per-worker results-cache path string the worker branch can read.
    assert isinstance(wi["mypy_config_stash_serialized"], str)
    assert "rstest-mypy-" in wi["mypy_config_stash_serialized"]


def test_seed_pytest_mypy_noop_without_plugin():
    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    wi = {}
    config = _RetryConfig({}, wi)  # no pytest-mypy
    _seed_pytest_mypy(config)
    assert "mypy_config_stash_serialized" not in wi


def test_seed_pytest_mypy_skips_when_xdist_present():
    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    wi = {}
    # real xdist installed -> pytest-mypy self-provisions via its controller
    config = _RetryConfig({"mypy": object(), "xdist": object()}, wi)
    _seed_pytest_mypy(config)
    assert "mypy_config_stash_serialized" not in wi


def test_seed_pytest_mypy_respects_module_name():
    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    wi = {}
    config = _RetryConfig({"pytest_mypy": object()}, wi)  # package-name variant
    _seed_pytest_mypy(config)
    assert isinstance(wi["mypy_config_stash_serialized"], str)


def test_seed_pytest_mypy_noop_when_key_already_set():
    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    wi = {"mypy_config_stash_serialized": "/pre/existing/path"}
    config = _RetryConfig({"mypy": object()}, wi)
    _seed_pytest_mypy(config)
    assert wi["mypy_config_stash_serialized"] == "/pre/existing/path"


def test_seed_pytest_mypy_noop_without_workerinput():
    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    config = _RetryConfig({"mypy": object()}, None)  # no workerinput attr
    _seed_pytest_mypy(config)  # must not raise
    assert getattr(config, "workerinput", None) is None


def test_seed_pytest_mypy_falls_back_to_neutralize_on_error(monkeypatch):
    import tempfile

    from rstest_worker._internal.plugincompat import _seed_pytest_mypy

    def _boom(*a, **k):
        raise OSError("no temp dir")

    monkeypatch.setattr(tempfile, "NamedTemporaryFile", _boom)
    plugin = object()
    config = _RetryConfig({"mypy": plugin}, {})
    _seed_pytest_mypy(config)
    # reservation failed -> plugin unregistered instead of leaving it to KeyError
    assert config.pluginmanager.unregistered == [plugin]


class _Dist:
    def __init__(self, name, version, requires):
        self.metadata = {"Name": name}
        self.project_name = name
        self.version = version
        self.requires = requires


def test_pin_conflict_flags_a_pin_that_excludes_the_running_pytest():
    (line,) = _pytest_pin_conflicts([(None, _Dist("pytest-old", "0.3", ["pytest<9,>=7"]))], "9.1.1")
    assert "pytest-old 0.3 requires pytest<9,>=7" in line
    assert "vendored pytest 9.1.1" in line


def test_pin_conflict_ignores_compatible_and_unrelated_requirements():
    dists = [
        (None, _Dist("pytest-asyncio", "1.4.0", ["pytest<10,>=8.4", "typing-extensions>=4.12"])),
        (None, _Dist("pytest-mock", "3.15.1", ["pytest>=6.2.5"])),
        (None, _Dist("pytest-bare", "1.0", ["pytest"])),
        (None, _Dist("no-reqs", "1.0", None)),
    ]
    assert _pytest_pin_conflicts(dists, "9.1.1") == []


def test_pin_conflict_ignores_extra_gated_and_unparseable_requirements():
    dists = [
        (None, _Dist("hypothesis", "6.0", ["pytest<9 ; extra == 'pytest'"])),
        (None, _Dist("weird", "1.0", ["pytest <<< 9"])),
    ]
    assert _pytest_pin_conflicts(dists, "9.1.1") == []


def test_pin_conflict_honors_active_markers():
    dist = _Dist("pytest-marked", "1.0", ["pytest<9 ; python_version >= '3'"])
    assert len(_pytest_pin_conflicts([(None, dist)], "9.1.1")) == 1


def test_pin_conflict_reports_each_distribution_once():
    # pluggy lists one (plugin, dist) pair per registered module.
    dist = _Dist("pytest-multi", "1.0", ["pytest<9"])
    assert len(_pytest_pin_conflicts([(object(), dist), (object(), dist)], "9.1.1")) == 1


class _PinConfig:
    def __init__(self, dists):
        self.pluginmanager = self
        self._dists = dists
        self.calls = 0

    def list_plugin_distinfo(self):
        self.calls += 1
        return self._dists


def test_warn_pytest_pins_prints_once_per_process(monkeypatch, capsys):
    monkeypatch.setattr(plugincompat, "_pins_checked", False)
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    config = _PinConfig([(None, _Dist("pytest-old", "0.3", ["pytest<9"]))])
    _warn_pytest_pins(config)
    _warn_pytest_pins(config)
    assert config.calls == 1
    assert capsys.readouterr().err.count("pytest-old 0.3") == 1


def test_warn_pytest_pins_only_on_gw0_in_the_pool(monkeypatch, capsys):
    monkeypatch.setattr(plugincompat, "_pins_checked", False)
    monkeypatch.setenv("RSTEST_WORKER_ID", "gw1")
    config = _PinConfig([(None, _Dist("pytest-old", "0.3", ["pytest<9"]))])
    _warn_pytest_pins(config)
    assert config.calls == 0
    assert capsys.readouterr().err == ""


def test_warn_pytest_pins_swallows_metadata_errors(monkeypatch, capsys):
    monkeypatch.setattr(plugincompat, "_pins_checked", False)
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)

    class _Broken:
        metadata = property(lambda self: 1 / 0)

    _warn_pytest_pins(_PinConfig([(None, _Broken())]))
    assert capsys.readouterr().err == ""
