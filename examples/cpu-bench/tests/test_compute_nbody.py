"""Pure-Python n-body integration: CPU-bound float arithmetic.

Five bodies (the outer solar system in AU / year units), semi-implicit Euler.
The integrator is symplectic, so total energy stays close to its start value;
the test checks that. Each case shifts the step size slightly so the items are
distinct; the step count, and so the cost, is the same for every case.
"""

import math

import pytest

STEPS = 88_000


def initial_bodies():
    # x, y, z, vx, vy, vz, mass
    return [
        [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 39.47],
        [4.84, -1.16, -0.10, 0.606, 2.81, -0.02, 0.037],
        [8.34, 4.12, -0.40, -1.01, 1.82, 0.008, 0.011],
        [12.89, -15.11, -0.22, 1.08, 0.868, -0.010, 0.0017],
        [15.37, -25.91, 0.179, 0.979, 0.594, -0.034, 0.0020],
    ]


def energy(bodies):
    e = 0.0
    for i, b in enumerate(bodies):
        e += 0.5 * b[6] * (b[3] ** 2 + b[4] ** 2 + b[5] ** 2)
        for c in bodies[i + 1 :]:
            e -= b[6] * c[6] / math.dist(b[:3], c[:3])
    return e


def advance(bodies, steps, dt):
    n = len(bodies)
    for _ in range(steps):
        for i in range(n):
            bi = bodies[i]
            for j in range(i + 1, n):
                bj = bodies[j]
                dx = bi[0] - bj[0]
                dy = bi[1] - bj[1]
                dz = bi[2] - bj[2]
                d2 = dx * dx + dy * dy + dz * dz
                mag = dt / (d2 * math.sqrt(d2))
                bi[3] -= dx * bj[6] * mag
                bi[4] -= dy * bj[6] * mag
                bi[5] -= dz * bj[6] * mag
                bj[3] += dx * bi[6] * mag
                bj[4] += dy * bi[6] * mag
                bj[5] += dz * bi[6] * mag
        for b in bodies:
            b[0] += dt * b[3]
            b[1] += dt * b[4]
            b[2] += dt * b[5]


@pytest.mark.parametrize("case", range(21))
def test_nbody_energy_conserved(case):
    bodies = initial_bodies()
    e0 = energy(bodies)
    advance(bodies, STEPS, dt=0.01 * (1 + case / 1000))
    assert abs(energy(bodies) - e0) / abs(e0) < 1e-2
