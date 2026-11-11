"""Warm-template workers (`--fork-pool=app`).

Two halves:

- **record** (pool workers, `RSTEST_WARM_RECORD=1`): at session end, list the
  modules this worker imported that a template could import ahead of the fork
  (not test modules, conftests, assertion-rewritten or rstest/pytest modules).
  The orchestrator stores them as the import manifest.
- **prewarm** (the fork-pool zygote, `RSTEST_WARM_MANIFEST=<path>`): import the
  manifest's modules before forking, under guards that refuse any module whose
  import side effect would be shared wrongly by every forked worker (a per-worker
  env read, a thread, a socket or other fd, a macOS system framework). A module
  that trips a guard is denied for next time, and this zygote starts over cold.
"""

from __future__ import annotations

import contextlib
import fnmatch
import importlib
import importlib._bootstrap as _bootstrap
import json
import os
import sys
import time
from typing import Any

RECORD_ENV = "RSTEST_WARM_RECORD"
MANIFEST_ENV = "RSTEST_WARM_MANIFEST"
RESULT_ENV = "RSTEST_WARM_RESULT"

# Top-level packages the zygote already has (or that are rstest's own).
_CORE = frozenset(
    {"pytest", "_pytest", "pluggy", "py", "iniconfig", "packaging", "rstest_worker"}
    | {"__main__", "__mp_main__"}
)

# Environment keys whose value differs per worker, or that only a worker's
# pytest session sets. A template reading one at import would hand every child
# the template's (absent) value.
WORKER_KEYS = frozenset(
    {
        "RSTEST_WORKER_ID",
        "RSTEST_SEND_IDS",
        "PYTEST_XDIST_WORKER",
        "PYTEST_XDIST_WORKER_COUNT",
        "PYTEST_XDIST_TESTRUNUID",
        "PYTEST_VERSION",
        "PYTEST_CURRENT_TEST",
    }
)

# Set at session end by StreamPlugin, sent with the worker's `done` event.
recorded: list[str] | None = None


# ── record ──────────────────────────────────────────────────────────────────


def record(session: Any) -> list[str]:
    """Modules in `sys.modules` (import order) a template may import."""
    config = session.config
    skip = {getattr(getattr(i, "module", None), "__name__", None) for i in session.items}
    skip |= {
        getattr(p, "__name__", None)
        for p in config.pluginmanager.get_plugins()
        if os.path.basename(getattr(p, "__file__", None) or "") == "conftest.py"
    }
    rewrite: set[str] = set()
    try:
        from _pytest.assertion import assertstate_key

        hook = config.stash[assertstate_key].hook
        skip |= set(hook._rewritten_names)
        rewrite = set(hook._must_rewrite)
    except Exception:
        pass
    try:
        pats = list(config.getini("python_files"))
    except Exception:
        pats = ["test_*.py", "*_test.py"]
    return filter_modules(sys.modules, skip, rewrite, pats, str(config.rootpath))


def coverage_active() -> bool:
    """coverage.py is measuring this worker (`--cov`, possibly from addopts)."""
    cov = sys.modules.get("coverage")
    try:
        return cov is not None and cov.Coverage.current() is not None
    except Exception:
        return False


def filter_modules(
    modules: dict[str, Any], skip: set[Any], rewrite: set[str], pats: list[str], rootdir: str
) -> list[str]:
    out = []
    for name, mod in list(modules.items()):
        path = getattr(mod, "__file__", None)
        if not path or name in skip or name.split(".", 1)[0] in _CORE:
            continue
        # Marked for assertion rewriting (pytest11 plugins, register_assert_rewrite):
        # imported here, pytest's rewrite hook would never see it.
        if any(name == r or name.startswith(r + ".") for r in rewrite):
            continue
        base = os.path.basename(path)
        if path.startswith(rootdir) and any(fnmatch.fnmatch(base, p) for p in pats):
            continue
        out.append(name)
    return out


# ── prewarm (zygote) ────────────────────────────────────────────────────────


class Hazard(BaseException):
    """A module's import did something a forked child must not inherit.
    BaseException so a module's own `except Exception` can't swallow it."""

    def __init__(self, name: str, reason: str):
        super().__init__(name, reason)
        self.name, self.reason = name, reason


_loading: list[str] = []
_armed = False


class _RecordingEnviron(os._Environ):
    """`os.environ` while the template imports: a read of a per-worker key is a
    hazard for the module being imported. Shares the real mapping's data, and
    is inert once disarmed (a module may keep a reference to it)."""

    def __getitem__(self, key):
        if _armed and key in WORKER_KEYS:
            raise Hazard(_loading[-1] if _loading else "?", f"reads {key} at import")
        return super().__getitem__(key)

    def __contains__(self, key):
        if _armed and key in WORKER_KEYS:
            raise Hazard(_loading[-1] if _loading else "?", f"reads {key} at import")
        return super().__contains__(key)


class _Blocker:
    """Meta-path finder that fails denied modules, so a module that imports
    a denied one is denied too instead of dragging it into the template."""

    def __init__(self, deny: dict[str, str]):
        self.deny = deny

    def find_spec(self, name, path=None, target=None):
        if name in self.deny:
            raise ImportError(f"imports denied module {name}")
        return None


def _threads() -> int | None:
    if sys.platform == "darwin":
        import ctypes

        buf = ctypes.create_string_buffer(96)  # struct proc_taskinfo
        lib = ctypes.CDLL("/usr/lib/libproc.dylib")
        if lib.proc_pidinfo(os.getpid(), 4, 0, buf, 96) != 96:  # PROC_PIDTASKINFO
            return None
        return int.from_bytes(buf.raw[84:88], sys.byteorder)  # pti_threadnum
    try:
        return len(os.listdir("/proc/self/task"))
    except OSError:
        return None


def _fds() -> dict[int, tuple[int, int]]:
    """Open fds -> (st_mode, open flags). The listing's own fd is gone by the
    time it is stat'ed, so it never shows up."""
    import fcntl

    out = {}
    for entry in os.listdir("/dev/fd"):
        fd = int(entry)
        with contextlib.suppress(OSError):
            out[fd] = (os.fstat(fd).st_mode, fcntl.fcntl(fd, fcntl.F_GETFL))
    return out


_dyld: Any = None


def _images() -> int:
    """Count of loaded dyld images (macOS); 0 elsewhere."""
    return _dyld._dyld_image_count() if _dyld is not None else 0


def _new_framework(start: int) -> str | None:
    import ctypes

    for i in range(start, _images()):
        raw = _dyld._dyld_get_image_name(i)
        name = ctypes.cast(raw, ctypes.c_char_p).value or b""
        path = name.decode(errors="replace")
        if path.startswith(("/System/Library/Frameworks/", "/System/Library/PrivateFrameworks/")):
            return path
    return None


def _probe() -> tuple[int | None, dict[int, tuple[int, int]], int]:
    return _threads(), _fds(), _images()


def hazard(before: tuple, after: tuple) -> str | None:
    """Why the change from `before` to `after` (`_probe` snapshots) makes a
    module unsafe to import ahead of the fork, or None."""
    import stat

    t0, fds0, img0 = before
    t1, fds1, img1 = after
    if t0 is not None and t1 is not None and t1 > t0:
        return f"starts {t1 - t0} thread(s) at import"
    for fd, (mode, flags) in fds1.items():
        if fd in fds0:
            continue
        if stat.S_ISSOCK(mode):
            return "opens a socket at import"
        if not (stat.S_ISREG(mode) and flags & os.O_ACCMODE == os.O_RDONLY):
            return "leaves a file descriptor open at import"
    if img1 > img0 and (fw := _new_framework(img0)):
        return f"loads {fw} at import"
    return None


def _guarded_find_and_load(orig):
    def find_and_load(name, import_):
        if name in sys.modules or not _armed:
            return orig(name, import_)
        before = _probe()
        _loading.append(name)
        try:
            mod = orig(name, import_)
        finally:
            _loading.pop()
        reason = hazard(before, _probe())
        if reason:
            raise Hazard(name, reason)
        return mod

    return find_and_load


def prewarm(manifest_path: str, result_path: str | None) -> bool:
    """Import the manifest's modules. Returns False when this process is no
    longer fit to fork from (a guard fired, or an error after imports began):
    the caller then starts over as a plain core zygote."""
    global _armed, _dyld
    result: dict[str, Any] = {"imported": 0, "denied": {}, "hazard": None, "error": None}
    t0 = time.perf_counter()
    clean = True
    try:
        with open(manifest_path, encoding="utf-8") as f:
            manifest = json.load(f)
        deny = dict(manifest.get("deny") or {})
        modules = [m for m in manifest["modules"] if m not in deny]
        recorded_path = list(manifest.get("sys_path") or [])
    except Exception as e:
        result["error"] = f"unreadable manifest: {e}"
        _write(result_path, result, t0)
        return True
    if sys.platform == "darwin":
        import ctypes

        _dyld = ctypes.CDLL(None)
        _dyld._dyld_get_image_name.restype = ctypes.c_void_p
    saved_path, saved_env = list(sys.path), os.environ
    orig_fal = _bootstrap._find_and_load  # ty: ignore[unresolved-attribute]
    blocker = _Blocker(deny)
    sys.path[:] = recorded_path + [p for p in saved_path if p not in recorded_path]
    env: Any = saved_env
    # Swapped, not cleared: the recorder shares the real mapping's data.
    os.environ = _RecordingEnviron(  # noqa: B003
        env._data, env.encodekey, env.decodekey, env.encodevalue, env.decodevalue
    )
    sys.meta_path.insert(0, blocker)
    _bootstrap._find_and_load = _guarded_find_and_load(orig_fal)  # ty: ignore[unresolved-attribute]
    _armed = True
    try:
        for name in modules:
            if name in sys.modules:
                continue
            try:
                importlib.import_module(name)
                result["imported"] += 1
            except Hazard as h:
                result["denied"][h.name] = h.reason
                result["hazard"] = f"{h.name}: {h.reason}"
                clean = False
                break
            except (Exception, SystemExit) as e:
                reason = f"{type(e).__name__}: {e}".splitlines()[0][:200]
                result["denied"][name] = f"import error ({reason})"
    except Exception as e:
        result["error"] = f"{type(e).__name__}: {e}"
        clean = result["imported"] == 0
    finally:
        _armed = False
        _bootstrap._find_and_load = orig_fal  # ty: ignore[unresolved-attribute]
        with contextlib.suppress(Exception):
            sys.meta_path.remove(blocker)
        os.environ = saved_env  # noqa: B003
        sys.path[:] = saved_path
    _write(result_path, result, t0)
    return clean


def _write(path: str | None, result: dict[str, Any], t0: float) -> None:
    if not path:
        return
    result["seconds"] = round(time.perf_counter() - t0, 3)
    try:
        with open(path, "w", encoding="utf-8") as f:
            json.dump(result, f)
    except OSError:
        pass


# ── forked child ─────────────────────────────────────────────────────────────


def reseed_after_fork() -> None:
    """Global RNGs that don't reseed on fork (stdlib `random` does): without
    this every warm-forked worker draws the same stream."""
    np_random = sys.modules.get("numpy.random")
    if np_random is not None:
        with contextlib.suppress(Exception):
            np_random.seed()
    torch = sys.modules.get("torch")
    if torch is not None:
        with contextlib.suppress(Exception):
            torch.seed()
