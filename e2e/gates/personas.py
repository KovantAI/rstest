"""e2e gate section: the persona specs.

The Gherkin features under e2e/personas/features are the source of truth for
the evaluator, migrator, daily-dev, CI and maintainer personas. They run under
pytest-bdd in this interpreter, so it needs the dev dependency group
(`uv sync --group dev`, or `pip install --group dev`).
"""

import importlib.util
import subprocess
import sys

from _harness import E2E, REPO, check


def gate_personas(g, args, binary):
    print("== personas: Gherkin specs (e2e/personas) ==")
    if importlib.util.find_spec("pytest_bdd") is None:
        check(
            "personas: pytest-bdd is importable",
            False,
            f"{sys.executable} lacks the dev group: run `uv run python e2e/gate.py` "
            "or `pip install --group dev`",
        )
        return
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
        ],
        cwd=REPO,
    )
    check(
        "personas: every persona scenario passes", r.returncode == 0, f"pytest exit {r.returncode}"
    )
