"""pytest plugin: record per-test outcomes to JSON, in rstest's report-json
shape, so `rstest --try` can diff a plain-pytest baseline against rstest.

Activate (rstest does this for you): run pytest with
`-p rstest_worker.recorder`; the output path comes from $RSTEST_RECORD.

The emitted `tests` map is keyed by nodeid with per-phase outcomes
(setup/call/teardown) plus call duration - the same fields rstest's
`--report-json` writes - so the two snapshots diff directly.
"""
import json
import os
import sys
import time


class _Recorder:
    def __init__(self):
        self.tests = {}  # nodeid -> {phase: outcome, "duration": float, ...}
        self.collect_errors = []
        self.t0 = time.time()

    def pytest_runtest_logreport(self, report):
        t = self.tests.setdefault(report.nodeid, {})
        t[report.when] = report.outcome  # "setup" | "call" | "teardown"
        if report.when == "call":
            t["duration"] = round(report.duration, 4)
        if getattr(report, "wasxfail", None) is not None:
            t["wasxfail"] = True

    def pytest_collectreport(self, report):
        if report.failed:
            self.collect_errors.append(report.nodeid)

    def pytest_sessionfinish(self, session, exitstatus):
        doc = {
            "meta": {
                "runner": "pytest",
                "kind": "recorder",
                "exitstatus": int(exitstatus),
                "wall": round(time.time() - self.t0, 3),
                "pytest": __import__("pytest").__version__,
                "python": sys.version.split()[0],
            },
            "collect_errors": self.collect_errors,
            "tests": self.tests,
        }
        path = os.environ.get("RSTEST_RECORD", "rstest-pytest-record.json")
        with open(path, "w") as f:
            json.dump(doc, f, sort_keys=True)


def pytest_configure(config):
    config.pluginmanager.register(_Recorder(), "rstest-recorder")
