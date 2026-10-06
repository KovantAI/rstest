#!/usr/bin/env python3
"""Refuse to run the action at a monorepo root.

A monorepo root run keeps a `.rstest_cache` per project, writes JUnit as
`junit.<slug>.xml` per project, and refuses `--cache-pull/--cache-push`. The
action caches `<working-directory>/.rstest_cache`, uploads one JUnit file and
passes those cache flags, so at a root it would save nothing useful, upload
nothing, starve the fail-ratio gate, or exit 1. Fail early with a pointer to
the per-project matrix recipe instead.

Mirrors rstest's own decision (crates/rstest-cli/src/run/mod.rs, the monorepo
branch; mono/discover.rs; config.rs `has_pytest_config`): the directory is a
monorepo root when the args name no existing path, the directory has no pytest
config of its own, and either `[tool.rstest] projects` matches at least one
subproject or the walk finds two or more. Keep the two in sync.

Exit 0: not a monorepo root (or nothing to decide). Exit 1: monorepo root.
"""

from __future__ import annotations

import argparse
import configparser
import fnmatch
import re
import shlex
import sys
from pathlib import Path

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - 3.10 runners
    tomllib = None

CONFIG_NAMES = (
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
)
MAX_DEPTH = 4
RECIPE = (
    "https://python-rstest.readthedocs.io/en/latest/guides/ci-quickstart/"
    "#worked-example-monorepo-on-ephemeral-ci"
)


def _load_toml(path: Path) -> dict | None:
    text = path.read_text(encoding="utf-8", errors="replace")
    if tomllib is not None:
        try:
            return tomllib.loads(text)
        except tomllib.TOMLDecodeError:
            return None
    # No tomllib: table headers are all that matters, plus the one key this
    # guard reads (`projects` under [tool.rstest]).
    doc: dict = {}
    headers = list(re.finditer(r"^\s*\[\s*([A-Za-z0-9_.\-]+)\s*\]", text, re.M))
    for i, m in enumerate(headers):
        node = doc
        for part in m.group(1).split("."):
            node = node.setdefault(part, {})
        if m.group(1) == "tool.rstest":
            end = headers[i + 1].start() if i + 1 < len(headers) else len(text)
            pm = re.search(r"^\s*projects\s*=\s*\[(.*?)\]", text[m.end() : end], re.M | re.S)
            if pm:
                node["projects"] = re.findall(r"""["']([^"']*)["']""", pm.group(1))
    return doc


def _ini_has(path: Path, section: str) -> bool:
    parser = configparser.ConfigParser(interpolation=None, strict=False)
    try:
        parser.read(path, encoding="utf-8")
    except configparser.Error:
        return False
    return parser.has_section(section)


def has_pytest_config(d: Path) -> bool:
    for name in CONFIG_NAMES:
        p = d / name
        if not p.is_file():
            continue
        if name in ("pytest.ini", ".pytest.ini"):
            return True
        if name in ("pytest.toml", ".pytest.toml"):
            if _load_toml(p) is not None:
                return True
        elif name == "pyproject.toml":
            doc = _load_toml(p)
            tool_pytest = ((doc or {}).get("tool") or {}).get("pytest")
            if isinstance(tool_pytest, dict) and (
                any(k != "ini_options" for k in tool_pytest) or "ini_options" in tool_pytest
            ):
                return True
        elif (name == "tox.ini" and _ini_has(p, "pytest")) or (
            name == "setup.cfg" and _ini_has(p, "tool:pytest")
        ):
            return True
    return False


def _pruned(d: Path) -> bool:
    name = d.name
    return (
        name.startswith(".")
        or name in ("__pycache__", "node_modules", "site-packages")
        or (d / "pyvenv.cfg").exists()
    )


def discover_projects(root: Path) -> list[Path]:
    found: list[Path] = []

    def walk(d: Path, depth: int) -> None:
        if depth > MAX_DEPTH:
            return
        try:
            entries = sorted(d.iterdir())
        except OSError:
            return
        for e in entries:
            if not e.is_dir() or _pruned(e):
                continue
            if has_pytest_config(e):
                found.append(e)  # a project owns its subtree
            else:
                walk(e, depth + 1)

    walk(root, 0)
    return found


def rstest_projects(root: Path) -> list[str] | None:
    """`[tool.rstest] projects` the way rstest reads settings: the nearest
    parseable pyproject.toml at or above root decides, even without a
    `[tool.rstest]` table (it is the project boundary)."""
    for d in (root, *root.parents):
        p = d / "pyproject.toml"
        if not p.is_file():
            continue
        doc = _load_toml(p)
        if doc is None:
            continue  # malformed: rstest warns and keeps looking
        rstest = (doc.get("tool") or {}).get("rstest")
        if not isinstance(rstest, dict):
            return None
        projects = rstest.get("projects")
        if not isinstance(projects, list):
            return None  # absent, or invalid (rstest ignores it)
        return [g for g in projects if isinstance(g, str)]
    return None


def names_a_selection(args: list[str], root: Path) -> bool:
    """Loose version of rstest's check: any non-option token naming an existing
    path (or @argsfile). Over-matching only skips the guard, the safe side."""
    for a in args:
        if a.startswith("-"):
            continue
        a = a[1:] if a.startswith("@") else a
        a = a.split("::", 1)[0]
        if a and (root / a).exists():
            return True
    return False


def is_monorepo_root(root: Path, args: list[str]) -> list[Path] | None:
    if names_a_selection(args, root) or has_pytest_config(root):
        return None
    projects = discover_projects(root)
    globs = rstest_projects(root)
    if globs is not None:
        rel = [
            p
            for p in projects
            if any(fnmatch.fnmatchcase(p.relative_to(root).as_posix(), g) for g in globs)
        ]
        return rel if len(rel) >= 1 else None
    return projects if len(projects) >= 2 else None


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dir", default=".")
    ap.add_argument("--label", default="", help="how to name the directory in the error")
    ap.add_argument("--args", default="", help="the action's args input (shell-quoted)")
    ns = ap.parse_args(argv)
    root = Path(ns.dir).resolve()
    try:
        args = shlex.split(ns.args)
    except ValueError:
        args = ns.args.split()
    projects = is_monorepo_root(root, args)
    if projects is None:
        return 0
    shown = ", ".join(p.relative_to(root).as_posix() for p in projects[:5])
    more = f" (+{len(projects) - 5} more)" if len(projects) > 5 else ""
    print(
        f"::error::working-directory '{ns.label or ns.dir}' is a monorepo root "
        f"(subprojects: {shown}{more}). The rstest action runs one project: its "
        "cache path, JUnit upload and fail-ratio gate don't fit a root run, which "
        "keeps a cache and a junit.<slug>.xml per project and refuses "
        "--cache-pull/--cache-push. Run one matrix job per project with "
        f"working-directory set to the project: {RECIPE}",
        file=sys.stdout,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
