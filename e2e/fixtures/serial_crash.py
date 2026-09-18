
# Serial phase after a NON-designate crash: gw1 hard-crashes once on
# test_par_crash (marker-guarded). The designate (gw0) survives, so once the
# parallel phase resolves (including gw1's respawn draining) the serial tests
# still run exclusively on the designate, after all parallel work.
import json
import os
import signal
import time

import pytest


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)


def _log(name):
    start = time.monotonic()
    time.sleep(0.15)
    path = os.environ["RSTEST_E2E_LOG"] + "." + (os.environ.get("RSTEST_WORKER_ID") or "main")
    with open(path, "a") as f:
        f.write(
            json.dumps(
                {
                    "name": name,
                    "worker": os.environ.get("RSTEST_WORKER_ID"),
                    "start": start,
                    "end": time.monotonic(),
                }
            )
            + "\n"
        )


def test_par_a(): _log("par_a")
def test_par_b(): _log("par_b")
def test_par_c(): _log("par_c")
def test_par_d(): _log("par_d")


def test_par_crash():
    # Crash on WHICHEVER worker runs this (pool dispatch is dynamic, so a fixed
    # worker id is unreliable). The marker guard makes it fire exactly once; the
    # test runs on a single worker, so only one crash happens. Whether that
    # worker is the designate (-> promotion) or not, the serial phase must still
    # run on the surviving/promoted designate.
    marker = os.environ.get("CRASH_MARKER")
    if marker and not os.path.exists(marker):
        open(marker, "w").close()
        _hard_crash()
    _log("par_crash")


@pytest.mark.serial
def test_serial_one(): _log("serial_one")


@pytest.mark.serial
def test_serial_two(): _log("serial_two")
