"""Combine and report coverage after a parallel run.

Workers run pytest-cov in its (collocated) worker mode: each saves a
suffixed `.coverage.*` data file and reports nothing. In pytest-xdist the
master session combines and reports; under rstest that role belongs to the
orchestrator, which invokes this tool with the original session args.

Exit code: 0, or 1 when --cov-fail-under is not met (matching pytest-cov).
"""

import sys


def parse(args):
    reports = []
    fail_under = None
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


def main(argv):
    import coverage

    reports, fail_under = parse(argv)

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
                pct = cov.html_report(directory=arg or None)
                print(f"Coverage HTML written to dir {arg or 'htmlcov'}")
            elif kind == "json":
                pct = cov.json_report(outfile=arg or None)
            elif kind == "lcov":
                pct = cov.lcov_report(outfile=arg or None)
            elif kind == "annotate":
                cov.annotate(directory=arg or None)
                pct = None
            else:
                print(f"rstest: unknown --cov-report kind: {kind!r}", file=sys.stderr)
                continue
        except coverage.CoverageException as exc:
            print(f"rstest: coverage report failed: {exc}", file=sys.stderr)
            return 1
        if fail_under is not None and pct is not None and pct < float(fail_under):
            print(
                f"FAIL Required test coverage of {fail_under}% not reached. "
                f"Total coverage: {pct:.2f}%"
            )
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
