
import json
import os

import pytest


def _log(name):
    w = os.environ["RSTEST_WORKER_ID"]
    with open(os.environ["SLOG"] + "." + str(os.getpid()), "a") as f:
        f.write(json.dumps({"t": name, "w": w}) + "\n")


@pytest.mark.xdist_group("dbpool")
def test_g3(): _log("grp.g3")


def test_free3(): _log("free.3")
