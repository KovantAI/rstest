"""Unit tests for the pytest recorder plugin (JSON snapshot of outcomes)."""

import json
import types

from rstest_worker import recorder


def _report(nodeid, when, outcome, duration=0.0, wasxfail=None):
    r = types.SimpleNamespace(nodeid=nodeid, when=when, outcome=outcome, duration=duration)
    if wasxfail is not None:
        r.wasxfail = wasxfail
    return r


def test_logreport_records_phase_outcomes():
    rec = recorder._Recorder()
    rec.pytest_runtest_logreport(_report("t.py::a", "setup", "passed"))
    rec.pytest_runtest_logreport(_report("t.py::a", "call", "failed", duration=1.23456))
    rec.pytest_runtest_logreport(_report("t.py::a", "teardown", "passed"))
    assert rec.tests["t.py::a"] == {
        "setup": "passed",
        "call": "failed",
        "teardown": "passed",
        "duration": 1.2346,  # call duration rounded to 4 places
    }


def test_logreport_duration_only_on_call_phase():
    rec = recorder._Recorder()
    rec.pytest_runtest_logreport(_report("t.py::a", "setup", "passed", duration=9.9))
    assert "duration" not in rec.tests["t.py::a"]


def test_logreport_marks_wasxfail():
    rec = recorder._Recorder()
    rec.pytest_runtest_logreport(_report("t.py::a", "call", "passed", wasxfail="reason"))
    assert rec.tests["t.py::a"]["wasxfail"] is True


def test_logreport_none_wasxfail_not_marked():
    rec = recorder._Recorder()
    rec.pytest_runtest_logreport(_report("t.py::a", "call", "passed", wasxfail=None))
    assert "wasxfail" not in rec.tests["t.py::a"]


def test_collectreport_records_only_failures():
    rec = recorder._Recorder()
    rec.pytest_collectreport(types.SimpleNamespace(failed=True, nodeid="bad.py"))
    rec.pytest_collectreport(types.SimpleNamespace(failed=False, nodeid="ok.py"))
    assert rec.collect_errors == ["bad.py"]


def test_sessionfinish_writes_report_json(tmp_path, monkeypatch):
    out = tmp_path / "rec.json"
    monkeypatch.setenv("RSTEST_RECORD", str(out))
    rec = recorder._Recorder()
    rec.pytest_runtest_logreport(_report("t.py::a", "call", "passed", duration=0.5))
    rec.pytest_collectreport(types.SimpleNamespace(failed=True, nodeid="bad.py"))

    rec.pytest_sessionfinish(session=None, exitstatus=1)

    doc = json.loads(out.read_text())
    assert doc["meta"]["runner"] == "pytest"
    assert doc["meta"]["kind"] == "recorder"
    assert doc["meta"]["exitstatus"] == 1
    assert doc["collect_errors"] == ["bad.py"]
    assert doc["tests"]["t.py::a"]["call"] == "passed"


def test_sessionfinish_defaults_path_without_env(tmp_path, monkeypatch):
    monkeypatch.delenv("RSTEST_RECORD", raising=False)
    monkeypatch.chdir(tmp_path)
    recorder._Recorder().pytest_sessionfinish(session=None, exitstatus=0)
    assert (tmp_path / "rstest-pytest-record.json").exists()


def test_configure_registers_recorder_plugin():
    registered = {}

    class FakePM:
        def register(self, plugin, name):
            registered["plugin"] = plugin
            registered["name"] = name

    config = types.SimpleNamespace(pluginmanager=FakePM())
    recorder.pytest_configure(config)
    assert registered["name"] == "rstest-recorder"
    assert isinstance(registered["plugin"], recorder._Recorder)
