"""wincpu against a fake kernel32, so its job-object logic runs on every OS."""

from __future__ import annotations

import ctypes
from types import SimpleNamespace

import pytest
from rstest_worker._internal import stream, wincpu


class FakeKernel32:
    """kernel32 stand-in: each API is a plain function so ctypes' restype /
    argtypes assignments land on it, as they do on a real WinDLL export."""

    def __init__(self, *, create=0x10, set_info=1, assign=1, user=0, kernel=0):
        self.closed = []
        self.limit_flags = None

        def CreateJobObjectW(attrs, name):
            return create

        def SetInformationJobObject(job, kind, ref, size):
            assert kind == wincpu._EXTENDED_LIMIT
            assert size == ctypes.sizeof(wincpu._ExtendedLimit)
            self.limit_flags = ref._obj.BasicLimitInformation.LimitFlags
            return set_info

        def AssignProcessToJobObject(job, proc):
            assert (job, proc) == (create, -1)
            return assign

        def QueryInformationJobObject(job, kind, ref, size, ret_len):
            assert kind == wincpu._ACCOUNTING
            ref._obj.TotalUserTime = user
            ref._obj.TotalKernelTime = kernel
            return 1

        def GetCurrentProcess():
            return -1

        def CloseHandle(handle):
            self.closed.append(handle)

        for fn in (
            CreateJobObjectW,
            SetInformationJobObject,
            AssignProcessToJobObject,
            QueryInformationJobObject,
            GetCurrentProcess,
            CloseHandle,
        ):
            setattr(self, fn.__name__, fn)


@pytest.fixture
def kernel32(monkeypatch):
    def install(**kw):
        k32 = FakeKernel32(**kw)
        monkeypatch.setattr(wincpu.sys, "platform", "win32")
        monkeypatch.setattr(ctypes, "WinDLL", lambda name, **_: k32, raising=False)
        return k32

    wincpu._job_clock.cache_clear()
    yield install
    wincpu._job_clock.cache_clear()


def test_job_cpu_seconds_sums_user_and_kernel_time(kernel32):
    # 100ns units: 1.5s user + 0.25s kernel.
    k32 = kernel32(user=15_000_000, kernel=2_500_000)
    assert wincpu.job_cpu_seconds() == pytest.approx(1.75)
    # Tests may break away from the job, as they could before the worker joined it.
    assert k32.limit_flags == wincpu._LIMIT_BREAKAWAY_OK
    assert k32.closed == []


def test_job_is_set_up_once(kernel32):
    kernel32()
    assert wincpu._job_clock() is wincpu._job_clock()


def test_no_job_when_create_fails(kernel32):
    k32 = kernel32(create=0)
    assert wincpu.job_cpu_seconds() is None
    assert k32.closed == []


@pytest.mark.parametrize("failing", ["set_info", "assign"])
def test_no_job_and_handle_closed_when_setup_fails(kernel32, failing):
    k32 = kernel32(**{failing: 0})
    assert wincpu.job_cpu_seconds() is None
    assert k32.closed == [0x10]


def test_cpu_now_reads_the_job_on_windows(monkeypatch):
    monkeypatch.setattr(stream.sys, "platform", "win32")
    monkeypatch.setattr(wincpu, "job_cpu_seconds", lambda: 7.0)
    assert stream._cpu_now() == 7.0


def test_cpu_now_falls_back_to_os_times_without_a_job(monkeypatch):
    monkeypatch.setattr(stream.sys, "platform", "win32")
    monkeypatch.setattr(wincpu, "job_cpu_seconds", lambda: None)
    monkeypatch.setattr(
        stream.os, "times", lambda: SimpleNamespace(children_user=0.0, children_system=0.0)
    )
    monkeypatch.setattr("time.process_time", lambda: 2.0)
    assert stream._cpu_now() == 2.0
