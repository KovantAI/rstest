"""Pure-Python prime sieve: CPU-bound, no I/O, no C-extension thread pools.

Every test does the same work (~0.25 s on an M4 Max), so no single long test
caps the scaling curve. The parameter only makes each test a distinct item.
"""

import pytest

N = 5_000_000
PI_N = 348_513  # number of primes <= 5,000,000


def sieve(n):
    flags = bytearray([1]) * (n + 1)
    flags[0] = flags[1] = 0
    count = 0
    for i in range(2, n + 1):
        if flags[i]:
            count += 1
            for j in range(i * i, n + 1, i):
                flags[j] = 0
    return count


@pytest.mark.parametrize("case", range(22))
def test_sieve(case):
    assert sieve(N) == PI_N
