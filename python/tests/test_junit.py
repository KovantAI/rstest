"""`RSTEST_JUNITXML`: pytest's own LogXML runs in the worker, but every
finished `<testcase>` is streamed over the protocol and the suite-level data
rides one `junit_suite` message; the worker never writes the file."""

from __future__ import annotations

import sys
import uuid
import xml.etree.ElementTree as ET
from typing import Any

import pytest
from _pytest.junitxml import xml_key
from rstest_worker._internal import junit, runner_pytest
from rstest_worker._internal.stream import StreamPlugin


class FakeConn:
    def __init__(self) -> None:
        self.sent: list[tuple[str, Any]] = []

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))


SRC = """
import pytest


@pytest.fixture
def broken_teardown():
    yield
    raise RuntimeError("teardown boom")


def test_pass(record_property):
    record_property("answer", 42)


def test_fail_then_teardown_error(broken_teardown):
    assert False


@pytest.fixture(scope="session", autouse=True)
def suite_props(record_testsuite_property):
    record_testsuite_property("build", "7")
"""


def _session(tmp_path, monkeypatch, *, env: bool = True, extra_files=(), args=()):
    name = f"test_junit_{uuid.uuid4().hex[:8]}"
    (tmp_path / f"{name}.py").write_text(SRC)
    for fname, text in extra_files:
        (tmp_path / fname).write_text(text)
    out = tmp_path / "out" / "junit.xml"
    if env:
        monkeypatch.setenv(junit.ENV, str(out))
    else:
        monkeypatch.delenv(junit.ENV, raising=False)
    monkeypatch.syspath_prepend(str(tmp_path))
    conn = FakeConn()
    runner_pytest._pytest_main(
        [
            str(tmp_path),
            "-q",
            "-p",
            "no:cacheprovider",
            "-p",
            "no:randomly",
            "-o",
            "junit_suite_name=mysuite",
            *args,
        ],
        plugins=[StreamPlugin(conn)],
    )
    sys.modules.pop(name, None)
    return conn.sent, out, name


def _cases(sent):
    return [
        (p["nodeid"], [ET.fromstring(c) for c in p["cases"]]) for k, p in sent if k == "junit_case"
    ]


def test_each_finished_testcase_is_streamed_and_no_file_is_written(tmp_path, monkeypatch):
    sent, out, name = _session(tmp_path, monkeypatch)
    cases = dict(_cases(sent))
    (passed,) = cases[f"{name}.py::test_pass"]
    assert passed.tag == "testcase"
    assert passed.get("name") == "test_pass"
    # record_property lands in the streamed element, exactly as under pytest.
    prop = passed.find("properties/property")
    assert prop is not None
    assert (prop.get("name"), prop.get("value")) == ("answer", "42")
    assert not out.exists()
    assert not out.parent.exists()


def test_call_failure_and_teardown_error_ship_as_one_message(tmp_path, monkeypatch):
    sent, _, name = _session(tmp_path, monkeypatch)
    nodeid = f"{name}.py::test_fail_then_teardown_error"
    msgs = [els for nid, els in _cases(sent) if nid == nodeid]
    # One message for the attempt, carrying both <testcase> elements.
    assert len(msgs) == 1
    failure, error = msgs[0]
    assert failure.find("failure") is not None
    assert error.find("error") is not None
    assert "teardown boom" in (error.find("error").text or "")


def test_suite_message_carries_name_properties_and_unfinalized_cases(tmp_path, monkeypatch):
    sent, _, _ = _session(
        tmp_path,
        monkeypatch,
        extra_files=[("test_broken_import.py", "import no_such_mod_xyz\n")],
        args=["--continue-on-collection-errors"],
    )
    suites = [p for k, p in sent if k == "junit_suite"]
    assert len(suites) == 1
    suite = suites[0]
    assert suite["name"] == "mysuite"
    assert suite["hostname"]
    assert "T" in suite["timestamp"]
    (prop,) = (ET.fromstring(p) for p in suite["properties"])
    assert (prop.get("name"), prop.get("value")) == ("build", "7")
    # The collection error is never finalized, so it rides the suite message,
    # and no already-streamed case is repeated there.
    extra = [ET.fromstring(x) for x in suite["extra"]]
    assert len(extra) == 1
    error = extra[0].find("error")
    assert error is not None
    assert "no_such_mod_xyz" in (error.text or "")


def test_no_env_means_no_junit_plugin_and_no_messages(tmp_path, monkeypatch):
    sent, _, _ = _session(tmp_path, monkeypatch, env=False)
    assert not [k for k, _ in sent if k.startswith("junit")]


class _Config:
    def __init__(self, stash):
        self.stash = stash
        self.registered: list[Any] = []
        self.pluginmanager = self

    def register(self, plugin, name):
        self.registered.append((plugin, name))


def test_maybe_register_keeps_a_users_own_junitxml(monkeypatch):
    # pytest already installed its LogXML (the user passed --junitxml through
    # some other door): ours must not replace it.
    monkeypatch.setenv(junit.ENV, "out.xml")
    theirs = object()
    config = _Config({xml_key: theirs})
    junit.maybe_register(config, FakeConn())
    assert config.stash[xml_key] is theirs
    assert config.registered == []


def test_each_testcase_is_streamed_once_and_not_resent_at_session_end(tmp_path, monkeypatch):
    # The orchestrator writes every streamed case plus the session's `extra`
    # ones: a finalized case left out of `_sent` lands in the file twice.
    conn = FakeConn()

    class _Register:
        def pytest_configure(self, config):
            junit.maybe_register(config, conn)

    (tmp_path / "test_a.py").write_text("def test_ok(): pass\ndef test_bad(): assert 0\n")
    monkeypatch.setenv(junit.ENV, str(tmp_path / "junit.xml"))
    monkeypatch.chdir(tmp_path)
    pytest.main(["-q", "-p", "no:cacheprovider", "test_a.py"], plugins=[_Register()])
    cases = [c for kind, p in conn.sent if kind == "junit_case" for c in p["cases"]]
    (suite,) = [p for kind, p in conn.sent if kind == "junit_suite"]
    assert len(cases) == 2
    assert suite["extra"] == []
