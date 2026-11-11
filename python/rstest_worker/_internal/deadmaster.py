"""Dead-master-path detector (`--warn-on-dead-master-path`).

A static scan of installed plugins and loaded conftests for code that branches
on "am I the xdist controller?". rstest has no Python controller: every worker
has a `workerinput`, so such code either reads a key nobody set (crash) or
waits for a controller branch that never runs (silent). Best-effort: nothing
here may ever break a run."""

from __future__ import annotations

import ast
import os
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

from rstest_worker._internal.plugincompat import PROVISIONED_KEYS, _is_dist_internal

ENV = "RSTEST_DEAD_MASTER_SCAN"

_INPUTS = ("workerinput", "slaveinput")

# xdist controller hooks rstest never calls.
_UNCALLED_HOOKS = frozenset(
    {
        "pytest_xdist_setupnodes",
        "pytest_xdist_node_collection_finished",
        "pytest_handlecrashitem",
        "pytest_xdist_make_scheduler",
    }
)

# Calls that make a controller-only branch matter: registering a plugin,
# writing a file, or scheduling session-end work. Terminal painting
# (`.write(`, `write_line`) is deliberately absent.
_SIDE_EFFECTS = frozenset({"register", "open", "write_text", "write_bytes", "dump", "add_cleanup"})

# Plugins rstest already handles or documents (docs/reference/top-100-plugins.md
# plus the SILENT_MASTER_FLAGS table in crates/rstest-cli/src/run/mod.rs):
# never scanned, so the warning can't contradict the matrix.
CURATED = frozenset(
    [
        "pytest-asyncio",
        "pytest-json-ctrf",
        "pytest-cov",
        "pytest-xdist",
        "pytest-mock",
        "pytest-timeout",
        "pytest-rerunfailures",
        "hypothesis",
        "pytest-metadata",
        "pytest-env",
        "pytest-httpx",
        "pytest-html",
        "pytest-django",
        "pytest-split",
        "pytest-repeat",
        "pytest-json-report",
        "pytest-benchmark",
        "pytest-socket",
        "syrupy",
        "pytest-unordered",
        "pytest-base-url",
        "pytest-randomly",
        "pytest-icdiff",
        "pytest-playwright",
        "pytest-homeassistant-custom-component",
        "pytest-dependency",
        "pytest-sugar",
        "allure-pytest",
        "pytest-custom-exit-code",
        "pytest-instafail",
        "pytest-postgresql",
        "pytest-recording",
        "pytest-order",
        "pytest-aiohttp",
        "pytest-dotenv",
        "pytest-subtests",
        "pytest-testmon",
        "pytest-check",
        "pytest-httpserver",
        "pytest-timeouts",
        "pytest-github-actions-annotate-failures",
        "pytest-bdd",
        "pytest-durations",
        "pytest-memray",
        "pytest-codspeed",
        "pytest-random-order",
        "pytest-factoryboy",
        "pytest-ordering",
        "pytest-snapshot",
        "pytest-retry",
        "pytest-docker",
        "pytest-vcr",
        "pytest-lazy-fixtures",
        "pytest-flakefinder",
        "pytest-profiling",
        "pytest-celery",
        "pytest-watcher",
        "pytest-datadir",
        "pytest-docker-tools",
        "pytest-test-groups",
        "pytest-testinfra",
        "pytest-flask",
        "pytest-freezegun",
        "pytest-freezer",
        "pytest-alembic",
        "pytest-pretty",
        "pytest-opentelemetry",
        "pytest-qt",
        "pytest-describe",
        "pytest-sftpserver",
        "pytest-regressions",
        "pytest-deadfixtures",
        "pytest-cases",
        "pytest-reportportal",
        "pytest-ansible",
        "pytest-find-dependencies",
        "pytest-watch",
        "pytest-azurepipelines",
        "pytest-picked",
        "pytest-anyio",
        "pytest-reportlog",
        "pytest-race",
        "pytest-assume",
        "pytest-md",
        "pytest-harvest",
        "pytest-cache",
        "pytest-doctestplus",
        "pytest-mpl",
        "pytest-variables",
        "pytest-clarity",
        "pytest-nunit",
        "pytest-pylint",
        "pytest-lazy-fixture",
        "pylint-pytest",
        "pytest-examples",
        "pytest-pytestrail",
        "pytest-mypy",
        "pytest-csv",
        "pytest-subprocess",
        "pytest-flake8",
    ]
)

# Top-level modules never scanned: pytest itself and rstest's own code.
_SKIP_TOP = frozenset({"pytest", "_pytest", "pluggy", "rstest_worker", "rstest"})

_DOCS = "docs/guides/plugins.md"


def _name(node: ast.AST) -> str | None:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        return node.attr
    return None


def _const(node: ast.AST) -> Any:
    return node.value if isinstance(node, ast.Constant) else None


def _is_input(node: ast.AST) -> bool:
    """`X.workerinput` or `getattr(X, "workerinput"[, default])`."""
    if isinstance(node, ast.Attribute):
        return node.attr in _INPUTS
    return (
        isinstance(node, ast.Call)
        and _name(node.func) == "getattr"
        and len(node.args) >= 2
        and _const(node.args[1]) in _INPUTS
    )


def _controller_when(test: ast.AST) -> bool | None:
    """True: the test holds only on the xdist controller. False: only on a
    worker. None: not a controller gate."""
    if isinstance(test, ast.UnaryOp) and isinstance(test.op, ast.Not):
        inner = _controller_when(test.operand)
        return None if inner is None else not inner
    if isinstance(test, ast.BoolOp) and isinstance(test.op, ast.And):
        return True if any(_controller_when(v) is True for v in test.values) else None
    if _is_input(test):
        return False
    if isinstance(test, ast.Call):
        name = _name(test.func)
        if name == "hasattr" and len(test.args) >= 2 and _const(test.args[1]) in _INPUTS:
            return False
        if name in ("is_xdist_controller", "is_xdist_master"):
            return True
        if name == "is_xdist_worker":
            return False
    if isinstance(test, ast.Compare) and len(test.ops) == 1:
        op, right = test.ops[0], test.comparators[0]
        if _is_input(test.left) and _const(right) is None and isinstance(right, ast.Constant):
            if isinstance(op, ast.Is):
                return True
            if isinstance(op, ast.IsNot):
                return False
        if _const(test.left) == "PYTEST_XDIST_WORKER":
            if isinstance(op, ast.NotIn):
                return True
            if isinstance(op, ast.In):
                return False
    return None


def _has_side_effect(stmts: list[ast.stmt]) -> bool:
    for stmt in stmts:
        for node in ast.walk(stmt):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith(
                "pytest_"
            ):
                return True
            if isinstance(node, ast.Call) and _name(node.func) in _SIDE_EFFECTS:
                # Swapping in a terminal reporter only paints the terminal.
                if any(_const(a) == "terminalreporter" for a in node.args):
                    continue
                return True
    return False


def scan_source(source: str) -> tuple[list[tuple[str, int, str | None]], set[str]]:
    """One file's findings, as `(kind, line, key)` with kind "crash" or
    "silent", plus the workerinput keys it assigns or `in`-checks itself (so
    are safe to read anywhere in the same plugin). Unparseable source: none."""
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError):
        return [], set()
    # Local names bound to a workerinput (`wi = config.workerinput`).
    aliases = {
        t.id
        for node in ast.walk(tree)
        if isinstance(node, ast.Assign) and _is_input(node.value)
        for t in node.targets
        if isinstance(t, ast.Name)
    }

    def is_input(node: ast.AST) -> bool:
        return _is_input(node) or (isinstance(node, ast.Name) and node.id in aliases)

    found: list[tuple[str, int, str | None]] = []
    owned: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Subscript) and is_input(node.value):
            key = _const(node.slice)
            if isinstance(key, str):
                if isinstance(node.ctx, ast.Load):
                    found.append(("crash", node.lineno, key))
                else:
                    owned.add(key)
        elif isinstance(node, ast.Compare) and isinstance(node.ops[0], (ast.In, ast.NotIn)):
            if is_input(node.comparators[0]) and isinstance(_const(node.left), str):
                owned.add(node.left.value)  # ty: ignore[unresolved-attribute]
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and (
            node.name in _UNCALLED_HOOKS
        ):
            found.append(("silent", node.lineno, None))
        for field in ("body", "orelse", "finalbody"):
            stmts = getattr(node, field, None)
            if isinstance(stmts, list):
                found.extend(_gated(stmts))
    return found, owned


def _gated(stmts: list[ast.stmt]) -> list[tuple[str, int, str | None]]:
    """Silent findings for controller-only branches in one statement list."""
    out: list[tuple[str, int, str | None]] = []
    for i, stmt in enumerate(stmts):
        if not isinstance(stmt, ast.If):
            continue
        when = _controller_when(stmt.test)
        if when is True:
            region = stmt.body
        elif when is False:
            # `if is_worker: return` guards the rest of the block.
            early_exit = isinstance(stmt.body[-1], (ast.Return, ast.Raise))
            region = stmt.orelse or (stmts[i + 1 :] if early_exit else [])
        else:
            continue
        if _has_side_effect(region):
            out.append(("silent", stmt.lineno, None))
    return out


def unit_findings(sources: list[str]) -> list[tuple[str, int, str | None]]:
    """Findings for one plugin or conftest (several files), with crash reads
    of keys that rstest or the unit itself provisions dropped."""
    found: list[tuple[str, int, str | None]] = []
    owned: set[str] = set()
    for src in sources:
        f, o = scan_source(src)
        found.extend(f)
        owned |= o
    safe = PROVISIONED_KEYS | owned
    return [f for f in found if not (f[0] == "crash" and f[2] in safe)]


def recommend(label: str, findings: Sequence[tuple[str, int, str | None]], conftest: bool) -> str:
    """The one warning line for a plugin (`label` = "<dist> <version>",
    `-p no:<name>` in plugin text) or a conftest (`label` = "<path>")."""
    crashes = [f for f in findings if f[0] == "crash"]
    if crashes:
        keys = ", ".join(f'workerinput["{k}"]' for k in dict.fromkeys(f[2] for f in crashes))
        head = f"{label}:{crashes[0][1]}" if conftest else label
        msg = (
            f"rstest: warning: {head} reads {keys}, which only an xdist controller sets; "
            "it will KeyError at -n >= 2. "
        )
        if conftest:
            return msg + (
                'Use config.workerinput.get("k") with a fallback, or set the value in a '
                "pytest_configure_node hook (rstest calls it per worker). "
                f"See {_DOCS}#plugins-that-expect-an-xdist-controller"
            )
        return msg + (
            "Run it at -n 0, or disable it in parallel runs with -p no:<name>, and report "
            f"the key so rstest can seed it. See {_DOCS}#plugins-that-expect-an-xdist-controller"
        )
    if conftest:
        return (
            f"rstest: warning: {label}:{findings[0][1]} runs only when there is no "
            "workerinput (the xdist controller); under rstest every worker has one, so this "
            "branch never runs at -n >= 2. Move the work to a per-worker hook, or aggregate "
            f"from rstest's --report-json. See {_DOCS}#the-silent-no-op-class"
        )
    return (
        f"rstest: warning: {label} does its work only on the xdist controller, which rstest "
        "does not run; its output is lost at -n >= 2. Use rstest's native --junitxml / "
        "--report-json / --html / --cov, or a separate -n 0 pass. "
        f"See {_DOCS}#the-silent-no-op-class"
    )


def _read(path: Any) -> str | None:
    try:
        return Path(path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None


def _canon(name: str) -> str:
    return name.lower().replace("_", "-").replace(".", "-")


def _plugin_lines(pm: Any) -> list[str]:
    out: list[str] = []
    by_dist: dict[str, tuple[Any, Any, set[str]]] = {}
    for plugin, dist in pm.list_plugin_distinfo():
        top = str(getattr(plugin, "__name__", "")).split(".", 1)[0]
        name = dist.metadata["Name"] or dist.project_name
        if not top or top in _SKIP_TOP or _is_dist_internal(plugin) or _canon(name) in CURATED:
            continue
        by_dist.setdefault(_canon(name), (plugin, dist, set()))[2].add(top)
    for plugin, dist, tops in by_dist.values():
        sources = [
            src
            for f in dist.files or ()
            if f.parts and (f.parts[0] in tops or f.parts[0][:-3] in tops) and f.suffix == ".py"
            if (src := _read(f.locate())) is not None
        ]
        found = unit_findings(sources)
        if found:
            name = dist.metadata["Name"] or dist.project_name
            line = recommend(f"{name} {dist.version}", found, conftest=False)
            out.append(line.replace("<name>", pm.get_name(plugin) or name))
    return out


def _conftest_lines(pm: Any, root: Path) -> list[str]:
    out: list[str] = []
    for plugin in pm.get_plugins():
        path = getattr(plugin, "__file__", None)
        if not path or os.path.basename(path) != "conftest.py":
            continue
        src = _read(path)
        found = unit_findings([src]) if src is not None else []
        if found:
            try:
                label = str(Path(path).relative_to(root))
            except ValueError:
                label = str(path)
            out.append(recommend(label, found, conftest=True))
    return out


_scanned = False


def warn_dead_master(config: Any) -> None:
    """Print the findings to stderr once per run (standalone session, or gw0 in
    the pool), when the orchestrator set `RSTEST_DEAD_MASTER_SCAN`."""
    global _scanned
    if _scanned or not os.environ.get(ENV):
        return
    if os.environ.get("RSTEST_WORKER_ID") not in (None, "gw0"):
        return
    _scanned = True
    try:
        pm = config.pluginmanager
        lines = _plugin_lines(pm) + _conftest_lines(pm, Path(config.rootpath))
    except Exception:
        return
    for line in lines:
        print(line, file=sys.stderr, flush=True)
