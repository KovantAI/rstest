"""Pure-Python dynamic programming: longest common subsequence.

An O(n*m) table fill in plain Python lists. For "ab"*k against "ba"*k the
answer is known in closed form (2k - 1), so the test checks the solver without
a second implementation. Every case does the same work.
"""

import pytest

K = 1230


def lcs(a, b):
    prev = [0] * (len(b) + 1)
    for ca in a:
        cur = [0]
        for j, cb in enumerate(b):
            cur.append(prev[j] + 1 if ca == cb else max(prev[j + 1], cur[j]))
        prev = cur
    return prev[-1]


@pytest.mark.parametrize("case", range(21))
def test_lcs(case):
    assert lcs("ab" * K, "ba" * K) == 2 * K - 1
