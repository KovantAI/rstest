import os
import pytest


# Every module/class fixture setup is one line in RSTEST_E2E_LOG (per worker
# file, see serial.py), so the gate can count them.
def _log(what):
    path = os.environ["RSTEST_E2E_LOG"] + "." + (os.environ.get("RSTEST_WORKER_ID") or "main")
    with open(path, "a") as f:
        f.write(what + "\n")


@pytest.fixture(scope="module")
def mod_fx():
    _log("module")


@pytest.fixture(scope="class")
def cls_fx():
    _log("class")


@pytest.mark.parametrize("i", range(6))
def test_par(i):
    pass


@pytest.mark.serial
def test_s1(mod_fx):
    pass


@pytest.mark.serial
def test_s2(mod_fx):
    pass


@pytest.mark.serial
class TestSerial:
    def test_c1(self, mod_fx, cls_fx):
        pass

    def test_c2(self, mod_fx, cls_fx):
        pass

    def test_c3(self, mod_fx, cls_fx):
        pass


@pytest.mark.serial
def test_s3(mod_fx):
    pass


@pytest.mark.serial
def test_flaky_last(mod_fx):
    # Fails once, then passes on rerun: the rerun reaches the designate after
    # it drained the serial phase.
    marker = os.environ["SERIAL_SCOPE_MARKER"]
    if not os.path.exists(marker):
        open(marker, "w").close()
        raise AssertionError("first attempt fails")
