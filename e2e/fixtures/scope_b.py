
import json
import os

import pytest


def _log(name):
    w = os.environ["RSTEST_WORKER_ID"]
    with open(os.environ["SLOG"] + "." + str(os.getpid()), "a") as f:
        f.write(json.dumps({"t": name, "w": w}) + "\n")


@pytest.mark.xdist_group("dbpool")
def test_g1(): _log("grp.g1")


def test_free1(): _log("free.1")
def test_free2(): _log("free.2")


@pytest.mark.xdist_group("dbpool")
def test_g2(): _log("grp.g2")
