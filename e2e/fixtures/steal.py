
# The BIG file: 40 items that each log their worker. Paired with a tiny second
# file so lazy runs 2 workers (it caps n to the file count). With --dist load
# (steal ON) the worker that finishes the tiny file steals from this file's
# own_queue, spreading these 40 items across >=2 workers. With --dist loadfile
# (steal OFF) file affinity keeps all 40 on their single collecting worker.
# Each item takes 50ms (2s total) so a thief that boots late (steal_conftest.py,
# or a loaded CI runner) still finds items left: instant items let the owner
# drain all 40 before the thief is ready, and nothing is left to steal.
import json
import os
import time

import pytest


def _log_worker():
    wid = os.environ.get("RSTEST_WORKER_ID") or "main"
    path = os.environ["RSTEST_E2E_LOG"] + "." + wid
    with open(path, "a") as f:
        f.write(json.dumps({"worker": wid}) + "\n")


@pytest.mark.parametrize("i", range(40))
def test_many(i):
    _log_worker()
    time.sleep(0.05)
