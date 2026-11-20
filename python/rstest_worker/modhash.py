"""Module-level hashes for `--incremental`'s import-closure guard.

A test is skipped only while every module its test file imports is unchanged,
because a module's import-time code (a constant, a registry) runs under no
test's coverage. Hashing those modules whole made any function-body edit in a
package whose `__init__` imports its submodules re-run the whole suite. A
module-level hash leaves out the bodies of functions, which do not run at
import, so it only moves when import-time behaviour can.

Kept in the hash: everything outside function bodies (module and class
statements, decorators, def headers with their defaults and annotations), and
the whole body of any function that ran outside a test's own call phase (at
import, or in a fixture's setup or teardown, which several tests may share).
Those are named by qualified name (`Class.method`, `#1`, `#2`... for repeats
of one name) so a later check keeps the same bodies even when lines move.

Reads a JSON request on stdin and writes the answer on stdout:

    {"root": "<dir>", "files": {"<rel>": {"lines": [<shared lines>]}
                                        | {"keep": ["<qualname>", ...]}}}
    -> {"<rel>": {"hash": "<sha256>", "keep": ["<qualname>", ...],
                  "defs": {"<qualname>": [<first>, <last>, "<sha256>"], ...}}}

`lines` (when recording) picks the bodies to keep from the lines that ran
outside a call phase; `keep` (when checking) reuses the recorded names. A file
that can't be read or parsed is left out of the answer: the caller falls back
to the whole-file hash.
"""

from __future__ import annotations

import ast
import hashlib
import json
import os
import sys

_COMPOUND = (ast.If, ast.Try, ast.With, ast.AsyncWith, ast.For, ast.AsyncFor, ast.While)
if sys.version_info >= (3, 11):
    _COMPOUND += (ast.TryStar,)


def module_hash(
    src: str, keep: set[str] | None = None, lines: set[int] | None = None
) -> tuple[str, list[str]]:
    """The module-level hash of `src` and the qualified names whose bodies it
    kept: those in `keep`, plus any function whose body holds one of `lines`."""
    digest, kept, _ = analyze(src, keep, lines)
    return digest, kept


def analyze(
    src: str, keep: set[str] | None = None, lines: set[int] | None = None
) -> tuple[str, list[str], dict[str, tuple[int, int, str]]]:
    """[`module_hash`], plus each left-out function body: qualified name ->
    (first body line, last line, hash of those lines). A test that covered
    lines in a body depends on that body's hash and the module-level hash."""
    tree = ast.parse(src)
    text = src.splitlines(keepends=True)
    keep = keep or set()
    lines = lines or set()
    drop: list[tuple[str, int, int]] = []
    kept: list[str] = []
    seen: dict[str, int] = {}

    def visit(body: list[ast.stmt], prefix: str) -> None:
        for node in body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                # Repeats of one name (a property's getter and setter, typing
                # overloads, conditional defs) are numbered in source order, so
                # a recorded name keeps exactly the body it named.
                name = prefix + node.name
                n = seen.get(name, 0)
                seen[name] = n + 1
                if n:
                    name = f"{name}#{n}"
                first, last = node.body[0].lineno, node.end_lineno or node.body[-1].lineno
                # A body that starts on its header's line (`def f(): return 1`)
                # can't be cut without the header: keep the whole def.
                same_line = text[first - 1][: node.body[0].col_offset].strip() != ""
                if same_line or name in keep or any(first <= ln <= last for ln in lines):
                    kept.append(name)
                else:
                    drop.append((name, first, last))
            elif isinstance(node, ast.ClassDef):
                visit(node.body, f"{prefix}{node.name}.")
            elif isinstance(node, _COMPOUND):
                for field in ("body", "orelse", "finalbody"):
                    visit(getattr(node, field, []), prefix)
                for handler in getattr(node, "handlers", []):
                    visit(handler.body, prefix)

    visit(tree.body, "")
    cut = set()
    defs: dict[str, tuple[int, int, str]] = {}
    for name, first, last in drop:
        cut.update(range(first, last + 1))
        body = hashlib.sha256()
        for line in text[first - 1 : last]:
            body.update(line.replace("\r\n", "\n").encode())
        defs[name] = (first, last, body.hexdigest())
    digest = hashlib.sha256()
    for n, line in enumerate(text, 1):
        if n not in cut:
            digest.update(line.replace("\r\n", "\n").encode())
    return digest.hexdigest(), sorted(set(kept)), defs


def main() -> None:
    request = json.load(sys.stdin)
    root = request["root"]
    out: dict[str, dict[str, object]] = {}
    for rel, spec in request["files"].items():
        try:
            with open(os.path.join(root, rel), encoding="utf-8") as fh:
                src = fh.read()
            digest, kept, defs = analyze(src, set(spec.get("keep", [])), set(spec.get("lines", [])))
        except (OSError, UnicodeDecodeError, SyntaxError, ValueError):
            continue
        out[rel] = {"hash": digest, "keep": kept, "defs": defs}
    json.dump(out, sys.stdout)


if __name__ == "__main__":
    main()
