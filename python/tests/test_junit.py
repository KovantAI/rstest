"""Unit tests for the worker's streaming junit capture."""

import pytest
from rstest_worker._internal import junit


class _Conn:
    def __init__(self):
        self.sent = []

    def send(self, kind, payload):
        self.sent.append((kind, payload))


def test_each_testcase_is_streamed_once_and_not_resent_at_session_end(tmp_path, monkeypatch):
    # The orchestrator writes every streamed case plus the session's `extra`
    # ones: a finalized case left out of `_sent` lands in the file twice.
    conn = _Conn()

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
