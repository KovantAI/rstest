"""BLAS-bound workloads for the worker x thread grid (excluded by default).

numpy hands matmul and solve to its BLAS library, which runs its own thread
pool. With several rstest workers each spinning up a full pool, the machine is
oversubscribed; OMP_NUM_THREADS / OPENBLAS_NUM_THREADS / MKL_NUM_THREADS cap
it. `measure.py --grid` sweeps workers x threads over these tests.

Run explicitly: `pytest -m blas` / `rstest -m blas` (needs numpy).
"""

import pytest

pytestmark = pytest.mark.blas

SIZE = 3000  # three 3000x3000 float64 matrices: ~216 MB working set per test
REPS = 3  # ~0.4 s per test with one BLAS thread on an M4 Max


@pytest.fixture
def np():
    # Imported per test, not at module level: a module-level importorskip would
    # show up as a skip in every default (`not blas`) run without numpy.
    return pytest.importorskip("numpy")


def _matrix(np, seed):
    return np.random.default_rng(seed).standard_normal((SIZE, SIZE))


@pytest.mark.parametrize("case", range(16))
def test_matmul(np, case):
    a = _matrix(np, case)
    b = _matrix(np, case + 1000)
    for _ in range(REPS):
        c = a @ b
    # Checked against a matrix-vector product, which is cheap next to matmul.
    v = np.ones(SIZE)
    np.testing.assert_allclose(c @ v, a @ (b @ v), rtol=1e-8, atol=1e-6)


@pytest.mark.parametrize("case", range(16))
def test_solve(np, case):
    a = _matrix(np, case) + SIZE * np.eye(SIZE)  # diagonally dominant: well-conditioned
    x = np.arange(SIZE, dtype=float)
    rhs = a @ x
    for _ in range(REPS):
        got = np.linalg.solve(a, rhs)
    np.testing.assert_allclose(got, x, rtol=1e-8, atol=1e-6)
