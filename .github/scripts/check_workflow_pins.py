#!/usr/bin/env python3
"""Fail if a GitHub workflow pulls in an unpinned dependency.

Checks every .github/workflows/*.yml for:
  - `uses:` actions not pinned to a full commit SHA (local `./` actions exempt),
  - `pip install` not hash-pinned, as OpenSSF Scorecard reads it: needs
    `--require-hashes` unless every argument is a `.whl` (`==` pins, `-r`
    files without hashes and `pip install .` all fail), and
    `pip install --group` (resolves ranges, bypassing uv.lock),
  - `uvx` tools without `@version` and `--with`/`--from` packages without a pin,
  - taiki-e/install-action `tool:` without `@version`,
  - astral-sh/setup-uv without a `version:` input,
  - no top-level `permissions:` (the job token would get the repo default),
  - release.yml not signing its assets (Sigstore bundle on the GitHub
    release, PEP 740 attestations on every PyPI publish).

Line-based on purpose (no PyYAML on a bare runner). Usage:
  python .github/scripts/check_workflow_pins.py [workflows_dir]
"""

from __future__ import annotations

import re
import shlex
import sys
from pathlib import Path

SHA_USES = re.compile(r"@[0-9a-f]{40}\b")
# pip options that take a value; the value is a file/group, not a package.
PIP_VALUE_OPTS = {"-r", "--requirement", "-c", "--constraint", "--target", "-t"}
UVX_VALUE_OPTS = {"--python", "-p", "--with-requirements", "--index-url", "--index"}


def _tokens(cmd: str) -> list[str]:
    try:
        # Backslashes are Windows path separators here, not escapes.
        return shlex.split(cmd.replace("\\", "/"), comments=True)
    except ValueError:
        return cmd.split()


def _is_pinned_pkg(tok: str) -> bool:
    return "==" in tok or "@" in tok


def _is_wheel(tok: str) -> bool:
    # A wheel path, or the pwsh `(Get-Item ...whl).FullName` resolving to one.
    return tok.endswith(".whl") or tok == "(Get-Item" or tok.endswith(".whl).FullName")


def check_pip(cmd: str) -> list[str]:
    toks = _tokens(cmd)
    if "install" not in toks:
        return []
    args = toks[toks.index("install") + 1 :]
    if "--group" in args:
        # Resolves pyproject ranges fresh; install from `uv export` instead.
        return ["--group (use uv export --frozen)"]
    if "--require-hashes" in args:
        return []
    # Without hashes only bare wheels count as pinned (Scorecard's rule).
    bad, skip = [], False
    for tok in args:
        if skip:
            bad.append(f"{tok} (add --require-hashes)")
            skip = False
        elif tok in PIP_VALUE_OPTS:
            skip = True
        elif not tok.startswith("-") and not _is_wheel(tok):
            bad.append(f"{tok} (add --require-hashes)")
    return bad


def check_uvx(cmd: str) -> list[str]:
    toks = _tokens(cmd)
    toks = toks[toks.index("uvx") + 1 :]
    bad, i, has_from = [], 0, False
    while i < len(toks):
        tok = toks[i]
        if tok in ("--with", "--from"):
            has_from |= tok == "--from"
            if i + 1 < len(toks) and not _is_pinned_pkg(toks[i + 1]):
                bad.append(toks[i + 1])
            i += 2
        elif tok in UVX_VALUE_OPTS:
            i += 2
        elif tok.startswith("-"):
            i += 1
        else:
            # The tool itself; with --from it is just the command name.
            if not has_from and not _is_pinned_pkg(tok):
                bad.append(tok)
            break
    return bad


def check_file(path: Path) -> list[str]:
    errors = []
    lines = path.read_text().splitlines()
    if not any(re.match(r"permissions:", line) for line in lines):
        errors.append(f"{path}: no top-level `permissions:` block")
    for n, line in enumerate(lines, 1):
        s = line.strip()
        if s.startswith("#"):
            continue
        where = f"{path}:{n}"
        m = re.match(r"-?\s*uses:\s*(\S+)", s)
        if m and not m.group(1).startswith("./"):
            if not SHA_USES.search(m.group(1)):
                errors.append(f"{where}: action not SHA-pinned: {m.group(1)}")
            if m.group(1).startswith("astral-sh/setup-uv@"):
                indent = len(line) - len(line.lstrip())
                block = []
                for nxt in lines[n:]:
                    if nxt.strip() and len(nxt) - len(nxt.lstrip()) <= indent:
                        break
                    block.append(nxt.strip())
                if not any(b.startswith("version:") for b in block):
                    errors.append(f"{where}: setup-uv without a pinned `version:`")
        m = re.match(r"tool:\s*(\S+)", s)
        if m and "@" not in m.group(1):
            errors.append(f"{where}: install-action tool not pinned: {m.group(1)}")
        for seg in re.split(r"&&|\|\||;|\|", s):
            seg = re.sub(r"^(-\s*)?(run:\s*)?", "", seg.strip())
            if re.search(r"\bpip install\b", seg):
                errors += [f"{where}: unpinned pip package: {p}" for p in check_pip(seg)]
            if re.match(r"uvx\b", seg):
                errors += [f"{where}: unpinned uvx package: {p}" for p in check_uvx(seg)]
    return errors


def check_release_signing(path: Path) -> list[str]:
    text = path.read_text()
    errors = []
    if not re.search(r"gh release create[^\n]*(\\\n[^\n]*)*\.sigstore\.json", text):
        errors.append(f"{path}: GitHub release does not attach a .sigstore.json bundle")
    publishes = text.count("pypa/gh-action-pypi-publish@")
    if text.count("attestations: true") < publishes:
        errors.append(f"{path}: a PyPI publish step lacks `attestations: true`")
    return errors


def main() -> int:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".github/workflows")
    errors = [e for f in sorted(root.glob("*.yml")) for e in check_file(f)]
    if (root / "release.yml").exists():
        errors += check_release_signing(root / "release.yml")
    print("\n".join(errors) or f"all workflow dependencies pinned ({root})")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
