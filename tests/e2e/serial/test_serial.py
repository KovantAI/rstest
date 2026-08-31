import json
import os
import time

import pytest


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


def test_par_a():
    _log("par_a")


def test_par_b():
    _log("par_b")


def test_par_c():
    _log("par_c")


def test_par_d():
    _log("par_d")


def test_par_e():
    _log("par_e")


def test_par_f():
    _log("par_f")


@pytest.mark.serial
def test_serial_one():
    _log("serial_one")


@pytest.mark.serial
def test_serial_two():
    _log("serial_two")
