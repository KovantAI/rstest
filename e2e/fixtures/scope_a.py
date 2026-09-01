
import json
import os


def _log(name):
    w = os.environ["RSTEST_WORKER_ID"]
    with open(os.environ["SLOG"] + "." + str(os.getpid()), "a") as f:
        f.write(json.dumps({"t": name, "w": w}) + "\n")


class TestAlpha:
    def test_a1(self): _log("alpha.a1")
    def test_a2(self): _log("alpha.a2")
    def test_a3(self): _log("alpha.a3")


class TestBeta:
    def test_b1(self): _log("beta.b1")
    def test_b2(self): _log("beta.b2")
    def test_b3(self): _log("beta.b3")
