"""e2e gate section: the persona specs.

The Gherkin features under e2e/personas/features are the source of truth for
the evaluator, migrator, daily-dev, CI and maintainer personas. They run under
pytest-bdd in this interpreter, so it needs the dev dependency group
(`uv sync --group dev`, or `pip install --group dev`).

One pytest run covers every feature; its junit report is split back into one
check per persona (feature file), naming the scenarios that failed.
"""

import importlib.util
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from collections import defaultdict
from pathlib import Path

from _harness import E2E, REPO, check


def _results_by_persona(junit: Path) -> dict:
    """persona -> (scenarios run, names of the failed ones)."""
    out = defaultdict(lambda: [0, []])
    for case in ET.parse(junit).iter("testcase"):
        props = {p.get("name"): p.get("value") for p in case.iter("property")}
        persona = props.get("persona", "<no persona>")
        out[persona][0] += 1
        if case.find("failure") is not None or case.find("error") is not None:
            out[persona][1].append(case.get("name"))
    return out


def gate_personas(g, args, binary):
    print("== personas: Gherkin specs (e2e/personas) ==", flush=True)
    if importlib.util.find_spec("pytest_bdd") is None:
        check(
            "personas: pytest-bdd is importable",
            False,
            f"{sys.executable} lacks the dev group: run `uv run python e2e/gate.py` "
            "or `pip install --group dev`",
        )
        return
    with tempfile.TemporaryDirectory() as tmp:
        junit = Path(tmp) / "personas.xml"
        r = subprocess.run(
            [
                sys.executable,
                "-m",
                "pytest",
                str(E2E / "personas"),
                "--binary",
                str(binary),
                "--venv",
                str(g.venv),
                "-q",
                f"--junitxml={junit}",
            ],
            cwd=REPO,
        )
        # Exit 0/1 means the specs ran; anything else (usage error, collection
        # error, interrupt) leaves no per-persona result to trust.
        if r.returncode not in (0, 1) or not junit.exists():
            check("personas: the persona specs ran", False, f"pytest exit {r.returncode}")
            return
        results = _results_by_persona(junit)
    features = sorted(p.stem for p in (E2E / "personas" / "features").glob("*.feature"))
    for persona in sorted(set(features) | set(results)):
        ran, failed = results.get(persona, (0, []))
        check(
            f"personas: {persona} ({ran} scenarios)",
            ran > 0 and not failed,
            f"failed: {', '.join(failed)}" if failed else "no scenario ran",
        )
