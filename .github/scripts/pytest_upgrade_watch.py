#!/usr/bin/env python3
"""Compare the vendored pytest version against the latest stable on PyPI.

Writes `outdated`, `latest`, `vendored` to $GITHUB_OUTPUT so the calling
workflow can decide whether to open a tracking issue. Stdlib only — runs on a
bare setup-python with no pip installs.
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
VERSION_FILE = REPO / "python" / "rstest_worker" / "_vendor" / "_pytest" / "_version.py"
PYPI = "https://pypi.org/pypi/pytest/json"

# A release segment plus an optional pre-release tag (aNbNrcN). We compare on
# the numeric release tuple and treat any pre-release as lower than its final.
_VER = re.compile(r"^(\d+(?:\.\d+)*)(?:(a|b|rc)(\d+))?$")
_PRE_RANK = {"a": 0, "b": 1, "rc": 2}


def parse(v: str) -> tuple | None:
    m = _VER.match(v.strip())
    if not m:
        return None
    release = tuple(int(x) for x in m.group(1).split("."))
    if m.group(2) is None:
        # final release sorts above any pre-release of the same release tuple
        return (release, 1, 0, 0)
    return (release, 0, _PRE_RANK[m.group(2)], int(m.group(3)))


def is_final(v: str) -> bool:
    m = _VER.match(v.strip())
    return bool(m) and m.group(2) is None


def vendored_version() -> str:
    text = VERSION_FILE.read_text(encoding="utf-8")
    m = re.search(r"^__version__ = version = ['\"]([^'\"]+)['\"]", text, re.MULTILINE)
    if not m:
        sys.exit(f"could not parse version from {VERSION_FILE}")
    return m.group(1)


def latest_stable() -> str:
    with urllib.request.urlopen(PYPI, timeout=30) as resp:
        data = json.load(resp)
    # Prefer the highest final release across all keys (info.version can be a
    # pre-release when that is the newest upload).
    finals = [v for v in data["releases"] if is_final(v)]
    if not finals:
        sys.exit("no final pytest releases found on PyPI")
    return max(finals, key=lambda v: parse(v) or ())


def emit(**kv) -> None:
    out = os.environ.get("GITHUB_OUTPUT")
    lines = [f"{k}={v}" for k, v in kv.items()]
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write("\n".join(lines) + "\n")
    for line in lines:
        print(line)


def main() -> None:
    vendored = vendored_version()
    latest = latest_stable()
    lv, vv = parse(latest), parse(vendored)
    assert lv is not None and vv is not None
    outdated = lv > vv
    emit(vendored=vendored, latest=latest, outdated=str(outdated).lower())


if __name__ == "__main__":
    main()
