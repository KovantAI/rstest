import json, os, time
import pytest

# Order-dependent pair: the polluter leaves process state behind and the victim
# fails if it shares a worker with the polluter and runs after it. Which of the
# two happens depends only on the schedule, which is what replay pins.


def _log(name):
    start = time.monotonic()
    # Per-worker file: cross-process append is not atomic on Windows.
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


def test_polluter():
    _log("polluter")
    os.environ["RSTEST_REPLAY_POLLUTED"] = "1"


def test_victim():
    _log("victim")
    assert "RSTEST_REPLAY_POLLUTED" not in os.environ


def test_other():
    _log("other")


def test_slow():
    # Keeps its worker busy so the other one drains first: the case where a
    # finished gw0 used to trip the id-carrier fallback and re-run everything.
    time.sleep(0.5)
    _log("slow")


@pytest.mark.flaky(reruns=2)
def test_always_fails():
    _log("always_fails")
    assert os.environ.get("RSTEST_REPLAY_FAIL") != "1"
