"""Unit tests for third-party-plugin neutralization helpers."""

import zlib

from rstest_worker._plugincompat import (
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
