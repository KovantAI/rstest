"""pytest's own junitxml, streamed to the orchestrator instead of a file.

rstest intercepts `--junitxml` (per-worker sessions would clobber one file)
and writes the merged document orchestrator-side. To keep that document
pytest's element for element, each worker still runs pytest's `LogXML` (so
`junit_family`, `junit_logging`, `junit_suite_name`, `--junit-prefix`,
`record_property`, `record_xml_attribute` and `record_testsuite_property`
all behave exactly as under pytest), but ships every finished `<testcase>`
over the protocol as it completes rather than writing at session end. A
worker that dies mid-run has already delivered everything it finished.
"""

from __future__ import annotations

import os
import platform
import xml.etree.ElementTree as ET
from typing import Any

from _pytest.junitxml import LogXML, xml_key

ENV = "RSTEST_JUNITXML"


def maybe_register(config: Any, conn: Any) -> None:
    """Install the streaming LogXML when the orchestrator asked for junit."""
    path = os.environ.get(ENV)
    if not path or config.stash.get(xml_key, None) is not None:
        return
    xml = StreamingLogXML(
        conn,
        path,
        config.option.junitprefix,
        config.getini("junit_suite_name"),
        config.getini("junit_logging"),
        config.getini("junit_duration_report"),
        config.getini("junit_family"),
        config.getini("junit_log_passing_tests"),
    )
    # pytest's record_* fixtures and its own unconfigure find it via the stash.
    config.stash[xml_key] = xml
    config.pluginmanager.register(xml, "rstest-junitxml")


class StreamingLogXML(LogXML):
    def __init__(self, conn: Any, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._conn = conn
        self._batch: list[str] | None = None
        self._sent: set[int] = set()

    def finalize(self, report: Any) -> None:
        nodeid = getattr(report, "nodeid", report)
        reporter = self.node_reporters.get((nodeid, getattr(report, "node", None)))
        super().finalize(report)
        if reporter is None:
            return
        self._sent.add(id(reporter))
        # finalize() swaps to_xml for a zero-arg lambda (pytest's own trick).
        case = ET.tostring(reporter.to_xml(), encoding="unicode")  # ty: ignore[missing-argument]
        if self._batch is not None:
            self._batch.append(case)
        else:
            self._conn.send("junit_case", {"nodeid": str(nodeid), "cases": [case]})

    def pytest_runtest_logreport(self, report: Any) -> None:
        # One message per test attempt: a call failure plus a teardown error
        # finalize as two <testcase> elements within the same teardown report.
        # The orchestrator keeps the last attempt per nodeid (reruns).
        self._batch = []
        try:
            super().pytest_runtest_logreport(report)
        finally:
            batch, self._batch = self._batch, None
        if batch:
            self._conn.send("junit_case", {"nodeid": report.nodeid, "cases": batch})

    def pytest_sessionfinish(self) -> None:
        # Never write the file here: the orchestrator does, from every worker.
        # Collection errors and internal errors are never finalized, so they
        # ride here, as do the suite-level attributes and properties.
        extra = [
            ET.tostring(r.to_xml(), encoding="unicode")  # ty: ignore[missing-argument]
            for r in self.node_reporters_ordered
            if id(r) not in self._sent
        ]
        properties = [
            ET.tostring(ET.Element("property", name=name, value=value), encoding="unicode")
            for name, value in self.global_properties
        ]
        self._conn.send(
            "junit_suite",
            {
                "name": self.suite_name,
                "timestamp": self.suite_start.as_utc().astimezone().isoformat(),
                "hostname": platform.node(),
                "properties": properties,
                "extra": extra,
            },
        )
