"""A wait-bound 'slow file' with duration skew.

The sleeps stand in for network calls, DB round-trips, and timeouts — no CPU
work, so overlapping them wins. The point of the *skew*: the longest test is
collected LAST. rstest's cold run has no timing data, so it dispatches in
collection order and picks the long pole up last — it then runs alone while the
other workers sit idle (the classic long-pole tail). The warm run knows the
durations and starts the long pole FIRST, so it overlaps with everything else.
That difference is the cold-vs-warm gap this example measures.
"""

import time

import pytest

# 40 medium tests, then one long pole — declaration order is collection order,
# so the 3.0s test is dispatched last on a cold (no-cache) run.
DURATIONS = [0.2] * 40 + [3.0]


@pytest.mark.parametrize("secs", DURATIONS)
def test_remote_call(secs):
    time.sleep(secs)  # simulated network/timeout-bound call
    assert secs >= 0
