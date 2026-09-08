"""Unit tests for third-party-plugin neutralization helpers."""

import zlib

from rstest_worker._internal.plugincompat import (
    _is_dist_internal,
    _neutralize_rerunfailures,
    _randomly_seed,
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
