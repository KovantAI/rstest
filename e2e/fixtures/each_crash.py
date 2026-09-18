
# --dist each crash: gw0 hard-crashes ONCE on test_crashes_once (marker-guarded
# so its respawn doesn't re-crash). The replacement must run only gw0's REMAINING
# item (test_other) from each_remnant, while gw1 runs the full suite untouched.
# Expected keyed outcomes: test_crashes_once [gw0] failed (fabricated, not
# retried), test_other [gw0] passed (remnant), both [gw1] passed.
import os
import signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)


def _maybe_crash():
    wid = os.environ.get("RSTEST_WORKER_ID")
    marker = os.environ.get("CRASH_MARKER")
    if wid == "gw0" and marker and not os.path.exists(marker):
        open(marker, "w").close()
        _hard_crash()


def test_crashes_once():
    _maybe_crash()
    assert True


def test_other():
    assert True
