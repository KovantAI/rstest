"""Windows: CPU time of this worker and every process it starts.

`os.times()` reports no child CPU on Windows, so a test that runs a CPU-heavy
tool through `subprocess.run` would read as waiting. A job object accounts CPU
for all its processes, running or exited, and a child joins its parent's job,
so the worker puts itself in a fresh job and reads the job's total instead.
"""

from __future__ import annotations

import ctypes
import functools
import sys
from collections.abc import Callable

_ACCOUNTING = 1  # JobObjectBasicAccountingInformation
_EXTENDED_LIMIT = 9  # JobObjectExtendedLimitInformation
_LIMIT_BREAKAWAY_OK = 0x800  # JOB_OBJECT_LIMIT_BREAKAWAY_OK


class _Accounting(ctypes.Structure):
    _fields_ = [
        ("TotalUserTime", ctypes.c_int64),  # 100ns units
        ("TotalKernelTime", ctypes.c_int64),
        ("ThisPeriodTotalUserTime", ctypes.c_int64),
        ("ThisPeriodTotalKernelTime", ctypes.c_int64),
        ("TotalPageFaultCount", ctypes.c_uint32),
        ("TotalProcesses", ctypes.c_uint32),
        ("ActiveProcesses", ctypes.c_uint32),
        ("TotalTerminatedProcesses", ctypes.c_uint32),
    ]


class _BasicLimit(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", ctypes.c_int64),
        ("PerJobUserTimeLimit", ctypes.c_int64),
        ("LimitFlags", ctypes.c_uint32),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", ctypes.c_uint32),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", ctypes.c_uint32),
        ("SchedulingClass", ctypes.c_uint32),
    ]


class _ExtendedLimit(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _BasicLimit),
        ("IoInfo", ctypes.c_uint64 * 6),  # IO_COUNTERS
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


@functools.cache
def _job_clock() -> Callable[[], float] | None:
    """Join a new job once; a reader of its CPU seconds, or None if that failed."""
    assert sys.platform == "win32"  # lets ty resolve ctypes.WinDLL
    from ctypes import wintypes

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.CreateJobObjectW.restype = wintypes.HANDLE
    k32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    k32.SetInformationJobObject.argtypes = [
        wintypes.HANDLE,
        ctypes.c_int,
        ctypes.c_void_p,
        wintypes.DWORD,
    ]
    k32.QueryInformationJobObject.argtypes = [
        wintypes.HANDLE,
        ctypes.c_int,
        ctypes.c_void_p,
        wintypes.DWORD,
        ctypes.c_void_p,
    ]
    k32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    k32.GetCurrentProcess.restype = wintypes.HANDLE
    k32.CloseHandle.argtypes = [wintypes.HANDLE]

    job = k32.CreateJobObjectW(None, None)
    if not job:
        return None
    # A test may start a process with CREATE_BREAKAWAY_FROM_JOB: keep that
    # working, as it did before the worker joined this job.
    limit = _ExtendedLimit()
    limit.BasicLimitInformation.LimitFlags = _LIMIT_BREAKAWAY_OK
    if not (
        k32.SetInformationJobObject(job, _EXTENDED_LIMIT, ctypes.byref(limit), ctypes.sizeof(limit))
        and k32.AssignProcessToJobObject(job, k32.GetCurrentProcess())
    ):
        k32.CloseHandle(job)
        return None
    info = _Accounting()

    def read() -> float:
        size = ctypes.sizeof(info)
        k32.QueryInformationJobObject(job, _ACCOUNTING, ctypes.byref(info), size, None)
        return (info.TotalUserTime + info.TotalKernelTime) / 1e7

    return read


def job_cpu_seconds() -> float | None:
    """CPU seconds of this process and its children (live or exited) since it
    joined its job, or None when no job could be set up."""
    clock = _job_clock()
    return clock() if clock is not None else None
