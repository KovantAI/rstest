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


# ── Dead-master-path detector (scan_dead_master_paths & helpers) ────────────

from rstest_worker._internal.plugincompat import (  # noqa: E402
    _code_tokens,
    _is_xdist_gated,
    _plugin_root,
    classify_master_path,
    scan_dead_master_paths,
)


def _mod_plugin(fn, name="thirdparty.plugin"):
    """Wrap hook functions in a module-like object named `name` so the scanner
    sees `pytest_*` attributes and a package root (like a real plugin module)."""
    import types as _t

    mod = _t.ModuleType(name)
    mod.__name__ = name
    for hook in fn if isinstance(fn, (list, tuple)) else [fn]:
        setattr(mod, hook.__name__, hook)
    return mod


# --- code-token scan ---


def test_code_tokens_finds_names_and_str_consts():
    def f(config):
        return getattr(config, "workerinput", None)  # 'workerinput' is a const

    tokens = _code_tokens(f.__code__)
    assert "workerinput" in tokens
    assert "getattr" in tokens  # co_names


def test_code_tokens_recurses_into_nested_code():
    # A token that lives ONLY inside a nested function/const must still surface.
    def outer():
        def inner(config):
            return config.workerinput["PYTEST_XDIST_WORKER"]

        return inner

    tokens = _code_tokens(outer.__code__)
    assert "workerinput" in tokens  # co_names, nested one level
    assert "PYTEST_XDIST_WORKER" in tokens  # str const, nested one level


def test_is_xdist_gated():
    assert _is_xdist_gated({"has_plugin", "xdist"})
    assert _is_xdist_gated({"numprocesses"})
    # has_plugin naming something other than xdist is not the gate:
    assert not _is_xdist_gated({"has_plugin", "cacheprovider"})
    assert not _is_xdist_gated({"workerinput"})


# --- classification ---


def test_classify_silent_noop_pytest_html_shape():
    # pytest-html: registers its writer only when NOT a worker; every rstest
    # worker has workerinput, so the writer never registers. No xdist gate.
    def pytest_configure(config):
        if not hasattr(config, "workerinput"):
            config._html = "register writer"

    plugin = _mod_plugin(pytest_configure, name="pytest_html.plugin")
    # (rename root off the vetted list to exercise classification directly)
    plugin.__name__ = "pytest_html_fork.plugin"
    result = classify_master_path(plugin)
    assert result == ("silent", ["pytest_configure"])


def test_classify_crash_precursor_retry_shape():
    # pytest-retry shape: master branch gated on has_plugin("xdist"), worker
    # branch reads workerinput["server_port"].
    def pytest_configure(config):
        if config.pluginmanager.has_plugin("xdist"):
            register = "xdist master hooks"  # noqa: F841
        elif hasattr(config, "workerinput"):
            port = config.workerinput["server_port"]  # noqa: F841

    plugin = _mod_plugin(pytest_configure, name="thirdparty_retry.plugin")
    assert classify_master_path(plugin) == ("crash", ["pytest_configure"])


def test_classify_none_when_no_workerinput_read():
    # xdist gate present but never reads workerinput -> not a dead worker branch.
    def pytest_configure(config):
        if config.pluginmanager.has_plugin("xdist"):
            _ = "something"

    plugin = _mod_plugin(pytest_configure, name="thirdparty_x.plugin")
    assert classify_master_path(plugin) is None


def test_classify_ignores_non_pytest_attrs():
    def helper(config):
        return config.workerinput  # not a pytest_* hook -> not scanned

    plugin = _mod_plugin(helper, name="thirdparty.plugin")
    assert classify_master_path(plugin) is None


# --- full scan + allowlist ---


def test_scan_reports_silent_and_skips_vetted():
    def pytest_configure(config):
        if not hasattr(config, "workerinput"):
            config._writer = 1

    def pytest_sessionfinish(session):  # vetted plugin, must be skipped
        _ = session.config.workerinput["cov"]

    offender = _mod_plugin(pytest_configure, name="pytest_html.plugin")
    offender.__name__ = "reportilizer.plugin"  # non-vetted root
    vetted = _mod_plugin(pytest_sessionfinish, name="pytest_django.plugin")

    findings = scan_dead_master_paths([offender, vetted])
    assert len(findings) == 1
    f = findings[0]
    assert f["cls"] == "silent"
    assert f["root"] == "reportilizer"
    assert f["hooks"] == ["pytest_configure"]


def test_scan_empty_for_clean_plugin():
    def pytest_configure(config):
        config.addinivalue_line("markers", "slow: mark")

    assert scan_dead_master_paths([_mod_plugin(pytest_configure)]) == []


def test_plugin_root_from_name_and_type():
    assert _plugin_root(_mod_plugin(lambda: None, name="foo.bar.baz")) == "foo"

    class Obj:
        pass

    Obj.__module__ = "pkgroot.sub"
    assert _plugin_root(Obj()) == "pkgroot"


def test_scan_skips_pytest_core_and_rstest_own():
    # _pytest.* internals (junitxml/stepwise) and rstest's own worker shim carry
    # the workerinput token by construction; warning on them is a false positive.
    def pytest_configure(config):
        if not hasattr(config, "workerinput"):
            config._writer = 1

    core = _mod_plugin(pytest_configure, name="_pytest.junitxml")
    own = _mod_plugin(pytest_configure, name="rstest_worker._internal.stream")
    # A same-shaped third-party plugin IS still reported (guards over-skipping).
    third = _mod_plugin(pytest_configure, name="reportilizer.plugin")

    findings = scan_dead_master_paths([core, own, third])
    assert [f["root"] for f in findings] == ["reportilizer"]
