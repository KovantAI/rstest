"""Recognize subtest reports (unittest `subTest`, the `subtests` fixture).

A subtest report carries its parent's nodeid and `when="call"`, so a consumer
keyed by (nodeid, phase) would let the parent's own report, sent after its
subtests, overwrite a subtest failure.
"""

from __future__ import annotations

# pytest >= 9 core (`_pytest.subtests.SubtestReport`) and the pytest-subtests
# plugin (`SubTestReport`).
_SUBTEST_REPORT_CLASSES = frozenset({"SubtestReport", "SubTestReport"})


def is_subtest_report(report) -> bool:
    return any(c.__name__ in _SUBTEST_REPORT_CLASSES for c in type(report).__mro__)


def failed_subtests(config, nodeid: str) -> int:
    """Failed `subtests`-fixture subtests of `nodeid` (pytest >= 9 core).

    pytest turns such a parent's passing call report into a failure ("contains
    N failed subtests") only while rendering its status line, which may run
    after other plugins already saw the report as passed. unittest `subTest`
    failures are not counted here: pytest leaves that parent passed.
    """
    try:
        from _pytest.subtests import failed_subtests_key
    except ImportError:
        return 0
    stash = getattr(config, "stash", None)
    counts = stash.get(failed_subtests_key, None) if stash is not None else None
    return counts.get(nodeid, 0) if counts else 0
