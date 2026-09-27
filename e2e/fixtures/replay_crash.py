import json, os, time


def _log(name):
    start = time.monotonic()
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


def test_before():
    _log("before")


def test_crash():
    _log("crash")
    os._exit(1)


def test_after_1():
    _log("after_1")


def test_after_2():
    _log("after_2")


def test_elsewhere():
    _log("elsewhere")
