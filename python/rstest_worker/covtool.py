"""Combine and report coverage after a parallel run.

Workers run pytest-cov in its (collocated) worker mode: each saves a
suffixed `.coverage.*` data file and reports nothing. In pytest-xdist the
master session combines and reports; under rstest that role belongs to the
orchestrator, which invokes this tool with the original session args.

When the session ran with `--cov-context=test`, the combined data carries
per-test line contexts (they survive the parallel merge - each worker records
into its own data file and `combine()` preserves the labels). This tool then
also (a) enables `show_contexts` so html/json reports surface them and
(b) writes a line->test index to `.rstest_cache/coverage_index.json` for
coverage-based `--changed` selection.

Exit code: 0, or 1 when --cov-fail-under is not met (matching pytest-cov).
"""

import hashlib
import json
import logging
import os
import sys
from typing import Any

log = logging.getLogger("rstest.covtool")

# Honor RSTEST_CACHE (the same override the Rust side reads via cache::dir) so
# the index is written where rstest reads it; defaults to CWD-relative .rstest_cache.
CACHE_DIR = os.environ.get("RSTEST_CACHE") or ".rstest_cache"
INDEX_PATH = os.path.join(CACHE_DIR, "coverage_index.json")
INDEX_SCHEMA = 2
# coverage labels dynamic contexts "<nodeid>|<phase>" (phase in run/setup/
# teardown); strip the phase to recover the bare nodeid.
_PHASE_SUFFIXES = ("|run", "|setup", "|teardown")


def parse(args: list[str]) -> tuple[list[str], str | None]:
    reports: list[str] = []
    fail_under: str | None = None
    it = iter(args)
    for a in it:
        if a == "--cov-report":
            reports.append(next(it, ""))
        elif a.startswith("--cov-report="):
            reports.append(a.split("=", 1)[1])
        elif a == "--cov-fail-under":
            fail_under = next(it, None)
        elif a.startswith("--cov-fail-under="):
            fail_under = a.split("=", 1)[1]
    if not reports:
        reports = ["term"]  # pytest-cov's default
    return [r for r in reports if r], fail_under


def _context_mode(args: list[str]) -> bool:
    """True when the session recorded per-test contexts (--cov-context)."""
    return any(a == "--cov-context" or a.startswith("--cov-context=") for a in args)


def _arg_value(args: list[str], name: str) -> "str | None":
    """Value of `--name value` or `--name=value`, or None."""
    it = iter(args)
    for a in it:
        if a == name:
            return next(it, None)
        if a.startswith(name + "="):
            return a.split("=", 1)[1]
    return None


def _fmt_ranges(lines: list[int]) -> str:
    """Compress a sorted line list to `1-3, 7, 10-12`."""
    out: list[str] = []
    i = 0
    while i < len(lines):
        j = i
        while j + 1 < len(lines) and lines[j + 1] == lines[j] + 1:
            j += 1
        out.append(str(lines[i]) if i == j else f"{lines[i]}-{lines[j]}")
        i = j + 1
    return ", ".join(out)


def diff_coverage(cov: Any, diff_lines_path: str, out_path: str) -> None:
    """Score coverage of the diff's ADDED lines: for each changed file, intersect
    its added line numbers with coverage.py's executable-statement analysis, and
    split into covered vs. missed. Non-executable added lines (blank/comment) are
    ignored. Writes `{pct, covered, uncovered, files:{path:[lines]}}` to
    `out_path` for the Rust gate, and prints a human summary."""
    with open(diff_lines_path) as f:
        diff: dict[str, list[int]] = json.load(f)

    total_cov = total_unc = 0
    files_out: dict[str, list[int]] = {}
    for rel, added_list in diff.items():
        added = set(added_list)
        if not added:
            continue
        try:
            # (filename, statements, excluded, missing, missing_formatted)
            _, statements, _excluded, missing, _ = cov.analysis2(os.path.abspath(rel))
        except Exception:
            continue  # file not measured (outside --cov, non-python, or absent)
        added_exec = added & set(statements)
        if not added_exec:
            continue
        uncovered = sorted(added_exec & set(missing))
        total_cov += len(added_exec) - len(uncovered)
        total_unc += len(uncovered)
        if uncovered:
            files_out[rel] = uncovered

    denom = total_cov + total_unc
    pct = 100.0 * total_cov / denom if denom else 100.0
    with open(out_path, "w") as f:
        json.dump({"pct": pct, "covered": total_cov, "uncovered": total_unc, "files": files_out}, f)
    if files_out:
        print(f"rstest: diff coverage {pct:.1f}% ({total_cov}/{denom} added lines covered)")
        for rel, lines in sorted(files_out.items()):
            print(f"  {rel}: uncovered added line(s) {_fmt_ranges(lines)}")
    elif denom:
        print(
            f"rstest: diff coverage {pct:.1f}% — all {total_cov} added executable line(s) covered"
        )
    else:
        print("rstest: diff coverage: no added executable lines to check")


def _base_nodeid(ctx: str) -> str:
    for suffix in _PHASE_SUFFIXES:
        if ctx.endswith(suffix):
            return ctx[: -len(suffix)]
    return ctx


def _file_sha256(path: str) -> str | None:
    """Hex SHA-256 of a file with CRLF normalized to LF, or None if unreadable.
    Newlines are normalized so the CRLF working tree (what coverage measured)
    hashes equal to the LF git blob the diff's line numbers come from."""
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError:
        return None
    return hashlib.sha256(data.replace(b"\r\n", b"\n")).hexdigest()


def build_index(cov: Any) -> None:
    """Invert the combined per-test contexts into a line->test index:
    { "schema": 2, "files": { "<rel-path>": { "hash": "<sha256>",
      "lines": { "<line>": ["<nodeid>", ...] } } } }.

    Keys are cwd-relative POSIX paths (matching git diff --relative), so files
    outside the tree are skipped. Best-effort: any error leaves the previous
    index untouched rather than failing the run.
    """
    data = cov.get_data()
    if not any(c for c in data.measured_contexts()):
        return  # nothing to index (contexts empty)
    # realpath both sides so a Windows 8.3 short name or junction doesn't make
    # an in-tree file look external and get skipped (coverage records
    # canonicalized paths; getcwd() may still carry the short form).
    cwd = os.path.realpath(os.getcwd())
    files: dict[str, dict[str, Any]] = {}
    for path in data.measured_files():
        try:
            rel = os.path.relpath(os.path.realpath(path), cwd)
        except ValueError:
            continue  # different drive on Windows -> not in the project tree
        if rel.startswith(".."):  # outside the project tree
            continue
        rel = rel.replace(os.sep, "/")
        line_map: dict[str, list[str]] = {}
        for line, ctxs in data.contexts_by_lineno(path).items():
            nodeids = sorted({_base_nodeid(c) for c in ctxs if c})
            if nodeids:
                line_map[str(line)] = nodeids
        if not line_map:
            continue
        # Stamp with the source hash. If the file vanished we can't vouch for
        # its line numbers, so drop it (selection falls back to the import graph).
        digest = _file_sha256(path)
        if digest is None:
            continue
        files[rel] = {"hash": digest, "lines": line_map}
    if not files:
        return
    os.makedirs(CACHE_DIR, exist_ok=True)
    tmp = INDEX_PATH + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump({"schema": INDEX_SCHEMA, "files": files}, fh)  # schema 2: {hash, lines}
    os.replace(tmp, INDEX_PATH)  # atomic swap so a reader never sees a partial file


def main(argv: list[str]) -> int:
    # coverage is provided by the user's project (pytest-cov / coverage), not a
    # declared worker dependency; imported lazily so the worker loads without it.
    import coverage

    reports, fail_under = parse(argv)
    context_mode = _context_mode(argv)

    cov = coverage.Coverage()
    # Suffixed worker data files exist after a pool run; a single-worker
    # run already wrote a plain .coverage.
    cov.combine(keep=False)
    cov.save()
    cov.load()

    status = 0
    for spec in reports:
        kind, _, arg = spec.partition(":")
        show_missing = kind == "term-missing"
        try:
            if kind in ("term", "term-missing"):
                pct = cov.report(show_missing=show_missing)
            elif kind == "xml":
                pct = cov.xml_report(outfile=arg or None)
                print(f"Coverage XML written to file {arg or 'coverage.xml'}")
            elif kind == "html":
                # show_contexts surfaces per-test contexts in the HTML report
                # (only meaningful under --cov-context; harmless otherwise).
                pct = cov.html_report(directory=arg or None, show_contexts=context_mode)
                print(f"Coverage HTML written to dir {arg or 'htmlcov'}")
            elif kind == "json":
                pct = cov.json_report(outfile=arg or None, show_contexts=context_mode)
            elif kind == "lcov":
                pct = cov.lcov_report(outfile=arg or None)
            elif kind == "annotate":
                cov.annotate(directory=arg or None)
                pct = None
            else:
                log.warning("unknown --cov-report kind: %r", kind)
                continue
        except coverage.CoverageException as exc:
            log.error("coverage report failed: %s", exc)
            return 1
        if fail_under is not None and pct is not None and pct < float(fail_under):
            print(
                f"FAIL Required test coverage of {fail_under}% not reached. "
                f"Total coverage: {pct:.2f}%"
            )
            status = 1

    # Build the line->test index from the merged contexts (coverage-based
    # --changed reads this). Best-effort - a failure here must not fail the run.
    if context_mode:
        try:
            build_index(cov)
        except Exception as exc:
            log.warning("coverage index build skipped: %s", exc)

    # Diff coverage: the Rust side hands us the diff's added lines and a result
    # path; we score them against the coverage data and it gates the exit code.
    diff_lines = _arg_value(argv, "--rstest-diff-lines")
    diff_out = _arg_value(argv, "--rstest-diff-out")
    if diff_lines and diff_out:
        try:
            diff_coverage(cov, diff_lines, diff_out)
        except Exception as exc:
            log.warning("diff coverage skipped: %s", exc)

    return status


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="rstest: %(message)s", stream=sys.stderr)
    sys.exit(main(sys.argv[1:]))
