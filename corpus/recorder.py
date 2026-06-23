"""pytest plugin: record per-test outcomes to JSON for compat diffing.
Activate: -p recorder (with this dir on PYTHONPATH). Output: $RSTEST_RECORD path."""
import json, os, platform, sys, time

class Recorder:
    def __init__(self):
        self.tests = {}   # nodeid -> {phase: outcome}, plus meta
        self.collect_errors = []
        self.t0 = time.time()

    def pytest_runtest_logreport(self, report):
        t = self.tests.setdefault(report.nodeid, {})
        t[report.when] = report.outcome
        if report.when == 'call':
            t['duration'] = round(report.duration, 4)
        if hasattr(report, 'wasxfail'):
            t['wasxfail'] = True
        if report.skipped and report.longrepr and isinstance(report.longrepr, tuple):
            t['skip_reason'] = str(report.longrepr[2])[:200]

    def pytest_collectreport(self, report):
        if report.failed:
            self.collect_errors.append(report.nodeid)

    def pytest_sessionfinish(self, session, exitstatus):
        out = {
            'meta': {
                'argv': sys.argv, 'python': sys.version.split()[0],
                'platform': platform.platform(), 'exitstatus': int(exitstatus),
                'wall': round(time.time() - self.t0, 2),
                'pytest': __import__('pytest').__version__,
            },
            'collect_errors': self.collect_errors,
            'tests': self.tests,
        }
        path = os.environ.get('RSTEST_RECORD', 'record.json')
        with open(path, 'w') as f:
            json.dump(out, f, sort_keys=True)

def pytest_configure(config):
    config.pluginmanager.register(Recorder(), 'rstest-recorder')
