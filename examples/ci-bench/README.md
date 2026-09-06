# Example: measuring cold vs warm

A tiny, self-contained suite that demonstrates — with **measured** numbers, not
projections — the three data points that matter when adopting rstest:

1. **pytest serial** — what CI pays today.
2. **rstest cold** — first run, no duration cache (what an ephemeral CI runner
   sees if nothing is persisted).
3. **rstest warm** — second run, duration-aware scheduling from cached timings
   (what you get once `.rstest_cache` is persisted between runs).

## The suite

136 tests, deliberately **wait-bound** (sleeps stand in for network/IO/timeouts
— no CPU work), with **duration skew**: `tests/test_api_slow.py` has 40 medium
tests followed by one long pole, plus 120 trivial unit tests across four files.

The long pole is collected *last*. A cold run has no timings, dispatches in
collection order, and picks the long pole up last — it then runs alone while
other workers idle. A warm run knows the durations and starts the long pole
first, overlapping it with everything else. That is the cold→warm win.

## Run it

```bash
# From this directory, with `rstest` and `pytest` on PATH:
python measure.py -n 4 --repeat 3

# Or point at a specific build / interpreter:
RSTEST=/path/to/rstest RSTEST_PYTHON=/path/to/venv/python \
  PYTEST="python -m pytest" python measure.py -n 4 --repeat 3
```

`-n 4` is fixed (not `auto`) so the result is comparable across machines —
`auto` would scale with core count and blur the cold-vs-warm point.

## Measured result

Apple Silicon, CPython 3.13, `-n 4`, best of 3:

| config | wall | vs pytest |
|---|---|---|
| pytest (serial) | 12.1s | 1.0× |
| rstest cold (`-n 4`, no cache) | 5.3s | 2.3× |
| rstest warm (`-n 4`, cached durations) | 3.6s | 3.3× |

Cold already wins from parallelism; warm adds ~1.5× on top by scheduling the
long pole first. Your numbers depend on your hardware and suite shape — this is
a synthetic wait-bound example, not a real application; run `measure.py` to get
your own. The [CI bench workflow](../../.github/workflows/example-bench.yml)
runs exactly this on GitHub's runners and posts the table to the job summary, so
the numbers above are reproducible on standard CI hardware too.
