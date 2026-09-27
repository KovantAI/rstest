#!/usr/bin/env python3
"""Run one command, time it, and (optionally) measure its memory.

Shared by the benchmarks (corpus/bench.py, examples/cpu-bench/measure.py) so
every published number comes from the same timing and memory code.

Two memory numbers, both for the whole process tree the command starts
(the rstest driver plus its workers, or pytest plus its xdist workers):

  max_proc_rss  — the largest single descendant's peak RSS, exact, from
                  getrusage(RUSAGE_CHILDREN).ru_maxrss. The kernel keeps the max
                  over every reaped descendant, so it has to be read in a fresh
                  process: the command runs under this file as a tiny wrapper
                  whose only child is the command.
  peak_tree_rss — the largest SUM of RSS over the whole tree, sampled with
                  psutil every `sample` seconds. Sampled, so a short spike
                  between two samples can be missed; None without psutil.

Wall time is taken inside the wrapper around the child alone, so the wrapper's
own interpreter start-up is not counted.

Sleep detection: the monotonic clock stops while the machine is suspended (a
laptop's idle sleep), the wall clock does not. A run whose wall-clock span
exceeds its monotonic span by more than SUSPEND_SLACK seconds straddled a
suspend; it comes back with `suspended: True` so callers can discard and
re-run it (`suspended()` does the same check for callers timing on their own).
"""

from __future__ import annotations

import contextlib
import json
import resource
import subprocess
import sys
import tempfile
import time
from pathlib import Path

try:
    import psutil
except ImportError:  # memory sampling is optional; timing never needs it
    psutil = None

SUSPEND_SLACK = 1.0  # seconds; clock-step noise is far below this

# ru_maxrss is bytes on macOS, kilobytes on Linux.
_MAXRSS_UNIT = 1 if sys.platform == "darwin" else 1024


def run_measured(cmd, cwd, env=None, *, sample=None, timeout=None):
    """Run `cmd`; return {rc, wall, stdout, stderr, max_proc_rss, peak_tree_rss}.

    `sample` (seconds) turns on the psutil tree sampler. Leave it None for
    timing runs, so the sampler's own CPU never touches a speed number. Memory
    values are bytes.
    """
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "result.json"
        wrapper = [sys.executable, str(Path(__file__).resolve()), str(out), "--", *cmd]
        # Output goes to files, not pipes: the sampler polls instead of reading,
        # and a chatty command would otherwise block on a full pipe buffer.
        with open(Path(tmp) / "stdout", "w+") as fo, open(Path(tmp) / "stderr", "w+") as fe:
            proc = subprocess.Popen(wrapper, cwd=cwd, env=env, stdout=fo, stderr=fe, text=True)
            peak_tree = None
            try:
                if sample and psutil is not None:
                    peak_tree = _sample_tree(proc, sample, timeout)
                proc.wait(timeout=timeout)
            except BaseException:
                proc.kill()
                proc.wait()
                raise
            fo.seek(0)
            fe.seek(0)
            stdout, stderr = fo.read(), fe.read()
        if not out.exists():
            raise RuntimeError(f"wrapper failed (rc={proc.returncode}): {stderr[-800:]}")
        res = json.loads(out.read_text())
    res.update(stdout=stdout, stderr=stderr, peak_tree_rss=peak_tree)
    return res


def _sample_tree(proc, interval, timeout):
    """Max over time of the summed RSS of every descendant of `proc` (the
    wrapper itself excluded: it is measurement overhead, not the command)."""
    assert psutil is not None  # caller gates on it
    root = psutil.Process(proc.pid)
    deadline = time.monotonic() + timeout if timeout else None
    peak = 0
    while proc.poll() is None:
        total = 0
        try:
            kids = root.children(recursive=True)
        except psutil.Error:
            break
        for p in kids:
            with contextlib.suppress(psutil.Error):  # exited between listing and reading
                total += p.memory_info().rss
        peak = max(peak, total)
        if deadline and time.monotonic() > deadline:
            break
        time.sleep(interval)
    return peak


def _wrapper_main(argv):
    out, sep, *cmd = argv
    assert sep == "--", argv
    t0 = time.perf_counter()
    c0 = time.time()
    # stdout/stderr are inherited, so the caller captures the command's output.
    rc = subprocess.call(cmd)
    wall = time.perf_counter() - t0
    clock = time.time() - c0
    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    Path(out).write_text(
        json.dumps(
            {
                "rc": rc,
                "wall": wall,
                "suspended": suspended(clock, wall),
                "max_proc_rss": ru.ru_maxrss * _MAXRSS_UNIT,
                "cpu_user": ru.ru_utime,
                "cpu_sys": ru.ru_stime,
            }
        )
    )
    return 0


def suspended(clock_span, monotonic_span):
    """True if the machine slept during a span timed on both clocks."""
    return clock_span - monotonic_span > SUSPEND_SLACK


def linear_fit(xs, ys):
    """Least-squares y = a + b*x; returns (a, b, r2). Used for the memory model
    total ~ orchestrator + N * per_worker."""
    n = len(xs)
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys, strict=True))
    b = sxy / sxx if sxx else 0.0
    a = my - b * mx
    ss_tot = sum((y - my) ** 2 for y in ys)
    ss_res = sum((y - (a + b * x)) ** 2 for x, y in zip(xs, ys, strict=True))
    r2 = 1 - ss_res / ss_tot if ss_tot else 1.0
    return a, b, r2


if __name__ == "__main__":
    sys.exit(_wrapper_main(sys.argv[1:]))
