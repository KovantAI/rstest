"""Inventory the pytest plugins installed in the running interpreter.

Run by corpus/run.py with a suite venv's python (stdlib only, so it works in
any venv). Prints one JSON list, sorted by name, of every distribution that
registers a `pytest11` plugin, plus the non-plugin libraries the docs' plugin
tables track (freezegun has no entry point; coverage backs pytest-cov):

    [{"name": ..., "version": ..., "requires_pytest": ...}, ...]

`requires_pytest` is the distribution's own `Requires-Dist` specifier on
pytest ("any" when unversioned, null when it has none), lower bounds first.
A requirement gated behind an extra is only used when there is no plain one,
and then `requires_pytest_extra` names that extra (hypothesis's `[pytest]`).
"""

import json
import re
from importlib import metadata

ALSO = {"freezegun", "coverage"}

_PYTEST_REQ = re.compile(r"\s*pytest(?![\w.-])\s*(.*)")
_EXTRA = re.compile(r"extra\s*==\s*['\"]([^'\"]+)['\"]")


def normalize(name):
    return re.sub(r"[-_.]+", "-", name).lower()


def _order(spec):
    """`<10,>=8.4` -> `>=8.4,<10`: lower bounds, then the rest, as written."""
    clauses = [c for c in spec.split(",") if c]
    return ",".join(sorted(clauses, key=lambda c: 0 if c.startswith(">") else 1))


def pytest_spec(requires):
    """`(specifier, extra)` for pytest in a `Requires-Dist` list: the plain
    requirement if there is one (extra None), else an extra-gated one
    (preferring an extra named `pytest`), else `(None, None)`."""
    gated = None
    for raw in requires or ():
        req, _, marker = raw.partition(";")
        m = _PYTEST_REQ.match(req)
        if not m:
            continue
        spec = _order(m.group(1).strip().strip("()").replace(" ", "")) or "any"
        extra = _EXTRA.search(marker)
        if extra is None:
            return spec, None
        if gated is None or extra.group(1) == "pytest":
            gated = (spec, extra.group(1))
    return gated or (None, None)


def inventory(dists):
    out = {}
    for dist in dists:
        name = dist.metadata["Name"]
        if not name:
            continue
        key = normalize(name)
        is_plugin = any(ep.group == "pytest11" for ep in dist.entry_points)
        if not is_plugin and key not in ALSO:
            continue
        spec, extra = pytest_spec(dist.requires)
        out[key] = {
            "name": key,
            "version": dist.version,
            "requires_pytest": spec,
            "requires_pytest_extra": extra,
        }
    return sorted(out.values(), key=lambda d: d["name"])


if __name__ == "__main__":
    print(json.dumps(inventory(metadata.distributions())))
