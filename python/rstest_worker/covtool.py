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

Reporting and the fail-under gate follow pytest-cov's own `summary` and
`should_fail_under` logic, reading the same coverage config (`--cov-config`,
`.coveragerc` `[report] fail_under` / `precision` / `show_missing`).

When the session ran in-process (`-n 0`/`-n 1`), pytest-cov already reported
and gated it; the orchestrator then passes `--rstest-cov-reported` and this tool
only builds the index, rewrites html/json with contexts, and scores the diff.

Exit code: 0, or 1 when the fail-under threshold is not met (matching pytest-cov).
"""

from __future__ import annotations

import hashlib
import io
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


def _arg_value(args: list[str], name: str) -> str | None:
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
            f"rstest: diff coverage {pct:.1f}% - all {total_cov} added executable line(s) covered"
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


def _index_base() -> str:
    """The directory index keys are relative to: the project rootdir rstest
    passes in `RSTEST_ROOTDIR`, else the cwd."""
    return os.environ.get("RSTEST_ROOTDIR") or os.getcwd()


def build_index(cov: Any) -> None:
    """Invert the combined per-test contexts into a line->test index:
    { "schema": 2, "files": { "<rel-path>": { "hash": "<sha256>",
      "lines": { "<line>": ["<nodeid>", ...] } } } }.

    Keys are POSIX paths relative to the project root (`RSTEST_ROOTDIR`, the
    rootdir the cache belongs to; the cwd when unset), like the nodeids, so a
    run from a subdirectory writes the same keys as a run from the root. Files
    outside the tree are skipped. Best-effort: any error leaves the previous
    index untouched rather than failing the run.
    """
    data = cov.get_data()
    if not any(c for c in data.measured_contexts()):
        return  # nothing to index (contexts empty)
    # realpath both sides so a Windows 8.3 short name or junction doesn't make
    # an in-tree file look external and get skipped (coverage records
    # canonicalized paths; getcwd() may still carry the short form).
    base = os.path.realpath(_index_base())
    files: dict[str, dict[str, Any]] = {}
    for path in data.measured_files():
        try:
            rel = os.path.relpath(os.path.realpath(path), base)
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


def _report_specs(reports: list[str]) -> dict[str, str | None]:
    """`--cov-report` specs as pytest-cov's `{kind: modifier-or-path}` map: a
    later spec of the same kind replaces an earlier one."""
    out: dict[str, str | None] = {}
    for spec in reports:
        kind, sep, arg = spec.partition(":")
        out[kind] = arg if sep else None
    return out


def _number(text: str) -> float:
    """pytest-cov's `--cov-fail-under` type: int, else float."""
    try:
        return int(text)
    except ValueError:
        return float(text)


def _write_reports(cov: Any, specs: dict[str, str | None], precision: int, contexts: bool) -> None:
    """Produce the requested reports the way pytest-cov's `summary` does: a
    fixed order (term, annotate, html, xml, json, markdown, lcov), not argv
    order, and `None` options left to the coverage config (`.coveragerc`
    `show_missing`, `skip_covered`, output paths)."""
    term = [k for k in ("term", "term-missing") if k in specs]
    if term:
        cov.report(
            show_missing=("term-missing" in specs) or None,
            skip_covered=("skip-covered" in specs.values()) or None,
            ignore_errors=True,
            precision=precision,
        )
    if "annotate" in specs:
        directory = specs["annotate"]
        cov.annotate(ignore_errors=True, directory=directory)
        if directory:
            print(f"Coverage annotated source written to dir {directory}")
        else:
            print("Coverage annotated source written next to source")
    # show_contexts surfaces per-test contexts in the html/json reports (only
    # meaningful under --cov-context; None keeps the config's value otherwise).
    show_contexts = True if contexts else None
    if "html" in specs:
        out = specs["html"]
        cov.html_report(ignore_errors=True, directory=out, show_contexts=show_contexts)
        print(f"Coverage HTML written to dir {cov.config.html_dir if out is None else out}")
    if "xml" in specs:
        out = specs["xml"]
        cov.xml_report(ignore_errors=True, outfile=out)
        print(f"Coverage XML written to file {cov.config.xml_output if out is None else out}")
    if "json" in specs:
        out = specs["json"]
        cov.json_report(ignore_errors=True, outfile=out, show_contexts=show_contexts)
        print(f"Coverage JSON written to file {cov.config.json_output if out is None else out}")
    for kind, mode, verb in (("markdown", "w", "written"), ("markdown-append", "a", "appended")):
        if kind in specs:
            out = specs[kind] or "coverage.md"
            with open(out, mode) as fh:
                cov.report(ignore_errors=True, file=fh, output_format="markdown")
            print(f"Coverage Markdown information {verb} to file {out}")
    if "lcov" in specs:
        out = specs["lcov"]
        cov.lcov_report(ignore_errors=True, outfile=out)
        print(f"Coverage LCOV written to file {cov.config.lcov_output if out is None else out}")
    unknown = set(specs) - {
        "term",
        "term-missing",
        "annotate",
        "html",
        "xml",
        "json",
        "markdown",
        "markdown-append",
        "lcov",
    }
    for kind in sorted(unknown):
        log.warning("unknown --cov-report kind: %r", kind)


def _rewrite_context_reports(cov: Any, specs: dict[str, str | None]) -> None:
    """In-process run under --cov-context: pytest-cov already wrote the
    html/json reports, but without per-test contexts; rewrite just those two
    (quietly: pytest-cov already announced them)."""
    if "html" in specs:
        cov.html_report(ignore_errors=True, directory=specs["html"], show_contexts=True)
    if "json" in specs:
        cov.json_report(ignore_errors=True, outfile=specs["json"], show_contexts=True)


def _gate(total: float, fail_under: float | int, precision: int) -> int:
    """pytest-cov's fail-under gate: the exit code follows coverage's
    precision-aware `should_fail_under`; the message line, like pytest-cov's,
    is printed whenever a positive threshold is set."""
    from coverage.results import display_covered, should_fail_under

    status = 0
    if should_fail_under(total, fail_under, precision):
        print(
            f"ERROR: Coverage failure: total of {display_covered(total, precision)} "
            f"is less than fail-under={fail_under:.{precision}f}"
        )
        status = 1
    if fail_under > 0:
        failed = total < fail_under
        print(
            "{fail}Required test coverage of {required}% {reached}. "
            "Total coverage: {actual:.2f}%".format(
                fail="FAIL " if failed else "",
                required=fail_under,
                reached="not reached" if failed else "reached",
                actual=total,
            )
        )
    return status


def main(argv: list[str]) -> int:
    # coverage is provided by the user's project (pytest-cov / coverage), not a
    # declared worker dependency; imported lazily so the worker loads without it.
    import coverage

    if "--no-cov" in argv:
        return 0  # pytest-cov is disabled: it measures, reports and gates nothing

    reports, fail_under_arg = parse(argv)
    specs = _report_specs(reports)
    context_mode = _context_mode(argv)
    # The session ran in-process with pytest-cov in its normal (non-worker)
    # mode and its output visible: pytest-cov already combined, reported and
    # applied --cov-fail-under, so reporting again would print everything twice.
    already_reported = "--rstest-cov-reported" in argv

    # The same config pytest-cov used (`--cov-config`, default `.coveragerc`,
    # which coverage treats as "search the usual files"): it sets the data
    # file location and the [report] defaults (fail_under, precision, ...).
    cov = coverage.Coverage(config_file=_arg_value(argv, "--cov-config") or ".coveragerc")
    try:
        if not already_reported:
            # Suffixed worker data files exist after a pool run.
            cov.combine(keep=False)
            cov.save()
        cov.load()
    except coverage.CoverageException as exc:
        log.warning("no coverage data to combine: %s", exc)

    status = 0
    if already_reported:
        if context_mode:
            try:
                _rewrite_context_reports(cov, specs)
            except Exception as exc:
                log.warning("coverage context report skipped: %s", exc)
    else:
        precision_arg = _arg_value(argv, "--cov-precision")
        precision = int(precision_arg) if precision_arg else int(cov.config.precision or 0)
        fail_under = (
            _number(fail_under_arg) if fail_under_arg is not None else cov.config.fail_under
        )
        # The total once, independent of which reports were asked for (none,
        # or only annotate, must still be gated), as pytest-cov computes it.
        try:
            total = cov.report(
                ignore_errors=True, output_format="total", precision=precision, file=io.StringIO()
            )
            _write_reports(cov, specs, precision, context_mode)
        except coverage.CoverageException as exc:
            print(f"WARNING: Failed to generate report: {exc}")
            total = 0.0
        if fail_under is not None:
            try:
                status = _gate(total, fail_under, precision)
            except coverage.CoverageException as exc:
                log.error("%s", exc)
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
