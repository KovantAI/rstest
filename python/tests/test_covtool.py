"""Unit tests for the coverage-combine tool's arg parsing and index helpers."""

import json
import os
import runpy

from rstest_worker import covtool


class _FakeData:
    """Minimal stand-in for coverage's CoverageData."""

    def __init__(self, contexts, files, per_file):
        self._contexts = contexts
        self._files = files
        self._per_file = per_file  # path -> {lineno: [ctx, ...]}

    def measured_contexts(self):
        return self._contexts

    def measured_files(self):
        return self._files

    def contexts_by_lineno(self, path):
        return self._per_file.get(path, {})


class _FakeCov:
    def __init__(self, data):
        self._data = data

    def get_data(self):
        return self._data


def test_parse_defaults_to_term():
    assert covtool.parse([]) == (["term"], None)


def test_parse_space_and_equals_forms():
    reports, fail_under = covtool.parse(
        ["--cov-report", "html:cov", "--cov-report=xml", "--cov-fail-under", "80"]
    )
    assert reports == ["html:cov", "xml"]
    assert fail_under == "80"


def test_parse_fail_under_equals_form():
    reports, fail_under = covtool.parse(["--cov-fail-under=90"])
    assert fail_under == "90"
    assert reports == ["term"]


def test_parse_drops_empty_report_specs():
    # An explicit empty --cov-report value disables reporting (pytest-cov
    # semantics): the term default applies only when no --cov-report is given.
    assert covtool.parse(["--cov-report="]) == ([], None)


def test_context_mode_detection():
    assert covtool._context_mode(["--cov-context=test"])
    assert covtool._context_mode(["--cov-context", "test"])
    assert not covtool._context_mode(["--cov-report=term"])


def test_base_nodeid_strips_phase_suffixes():
    assert covtool._base_nodeid("t.py::test_a|run") == "t.py::test_a"
    assert covtool._base_nodeid("t.py::test_a|setup") == "t.py::test_a"
    assert covtool._base_nodeid("t.py::test_a|teardown") == "t.py::test_a"


def test_base_nodeid_leaves_bare_id():
    assert covtool._base_nodeid("t.py::test_a") == "t.py::test_a"


def test_file_sha256_normalizes_crlf(tmp_path):
    # The CRLF working tree must hash equal to the LF git blob.
    crlf = tmp_path / "crlf.py"
    lf = tmp_path / "lf.py"
    crlf.write_bytes(b"a = 1\r\nb = 2\r\n")
    lf.write_bytes(b"a = 1\nb = 2\n")
    assert covtool._file_sha256(str(crlf)) == covtool._file_sha256(str(lf))


def test_file_sha256_missing_file_returns_none():
    assert covtool._file_sha256("/no/such/file/here.py") is None


def test_fmt_ranges_compresses_runs():
    assert covtool._fmt_ranges([1, 2, 3, 7, 10, 11, 12]) == "1-3, 7, 10-12"
    assert covtool._fmt_ranges([5]) == "5"
    assert covtool._fmt_ranges([]) == ""


def test_arg_value_reads_space_and_equals_forms():
    assert covtool._arg_value(["--x", "v"], "--x") == "v"
    assert covtool._arg_value(["--x=v"], "--x") == "v"
    assert covtool._arg_value(["--y", "v"], "--x") is None


class _FakeAnalysisCov:
    """Stand-in for coverage.Coverage with analysis2 keyed by abspath.

    `per_file` maps rel path -> (statements, missing). analysis2 raises for any
    path not present (mirrors coverage.py for unmeasured files)."""

    def __init__(self, per_file):
        # key by abspath, since diff_coverage calls analysis2(os.path.abspath(rel)).
        self._by_abs = {os.path.abspath(k): v for k, v in per_file.items()}

    def analysis2(self, abspath):
        stmts, missing = self._by_abs[abspath]  # KeyError -> caught by diff_coverage
        return (abspath, stmts, [], missing, "")


def _run_diff_cov(tmp_path, cov, diff):
    lines = tmp_path / "difflines.json"
    lines.write_text(json.dumps(diff))
    out = tmp_path / "diffout.json"
    covtool.diff_coverage(cov, str(lines), str(out))
    return json.loads(out.read_text())


def test_diff_coverage_all_covered(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    cov = _FakeAnalysisCov({"a.py": ([1, 2, 3], [])})
    res = _run_diff_cov(tmp_path, cov, {"a.py": [1, 2, 3]})
    assert res == {"pct": 100.0, "covered": 3, "uncovered": 0, "files": {}}


def test_diff_coverage_partial_reports_uncovered_lines(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # Added lines 1,2,3,4; statements are 1,2,3,4; lines 3,4 missing.
    cov = _FakeAnalysisCov({"a.py": ([1, 2, 3, 4], [3, 4])})
    res = _run_diff_cov(tmp_path, cov, {"a.py": [1, 2, 3, 4]})
    assert res["covered"] == 2
    assert res["uncovered"] == 2
    assert res["pct"] == 50.0
    assert res["files"] == {"a.py": [3, 4]}


def test_diff_coverage_ignores_non_executable_added_lines(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # Added lines 1..5 but only 2 and 4 are executable statements; 4 is missing.
    cov = _FakeAnalysisCov({"a.py": ([2, 4], [4])})
    res = _run_diff_cov(tmp_path, cov, {"a.py": [1, 2, 3, 4, 5]})
    assert res["covered"] == 1  # line 2
    assert res["uncovered"] == 1  # line 4
    assert res["files"] == {"a.py": [4]}


def test_diff_coverage_skips_unmeasured_file(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # b.py is not in the cov data -> analysis2 raises -> skipped silently.
    cov = _FakeAnalysisCov({"a.py": ([1], [])})
    res = _run_diff_cov(tmp_path, cov, {"a.py": [1], "b.py": [1, 2]})
    assert res == {"pct": 100.0, "covered": 1, "uncovered": 0, "files": {}}


def test_diff_coverage_skips_empty_added_list(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    cov = _FakeAnalysisCov({"a.py": ([1], [])})
    res = _run_diff_cov(tmp_path, cov, {"a.py": []})
    assert res == {"pct": 100.0, "covered": 0, "uncovered": 0, "files": {}}


def test_diff_coverage_no_executable_added_lines(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # Added lines are all non-executable (blank/comment): none intersect stmts.
    cov = _FakeAnalysisCov({"a.py": ([10, 11], [])})
    res = _run_diff_cov(tmp_path, cov, {"a.py": [1, 2, 3]})
    # denom == 0 -> pct defaults to 100.0, nothing reported.
    assert res == {"pct": 100.0, "covered": 0, "uncovered": 0, "files": {}}


def _index_cache(monkeypatch, tmp_path):
    """Point covtool's cache paths at tmp_path and return the index file."""
    cache = tmp_path / ".rstest_cache"
    index = cache / "coverage_index.json"
    monkeypatch.setattr(covtool, "CACHE_DIR", str(cache))
    monkeypatch.setattr(covtool, "INDEX_PATH", str(index))
    return index


def test_build_index_writes_line_to_test_map(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    index = _index_cache(monkeypatch, tmp_path)
    src = tmp_path / "mod.py"
    src.write_text("a = 1\nb = 2\n")

    data = _FakeData(
        contexts=["mod.py::test_a|run"],
        files=[str(src)],
        per_file={str(src): {1: ["mod.py::test_a|run"], 2: ["mod.py::test_b|setup"]}},
    )
    covtool.build_index(_FakeCov(data))

    doc = json.loads(index.read_text())
    assert doc["schema"] == covtool.INDEX_SCHEMA
    entry = doc["files"]["mod.py"]
    assert entry["hash"] == covtool._file_sha256(str(src))
    # phase suffixes stripped, nodeids sorted per line
    assert entry["lines"] == {"1": ["mod.py::test_a"], "2": ["mod.py::test_b"]}


def test_build_index_keys_are_relative_to_the_rootdir(tmp_path, monkeypatch):
    # A run from tests/unit/ shares the rootdir's cache: its keys must be
    # rootdir-relative (like the nodeids), not relative to the cwd.
    sub = tmp_path / "tests" / "unit"
    sub.mkdir(parents=True)
    monkeypatch.chdir(sub)
    monkeypatch.setenv("RSTEST_ROOTDIR", str(tmp_path))
    index = _index_cache(monkeypatch, tmp_path)
    src = tmp_path / "app" / "core.py"
    src.parent.mkdir()
    src.write_text("a = 1\n")
    nodeid = "tests/unit/test_u.py::test_u"
    data = _FakeData(
        contexts=[f"{nodeid}|run"],
        files=[str(src)],
        per_file={str(src): {1: [f"{nodeid}|run"]}},
    )
    covtool.build_index(_FakeCov(data))

    doc = json.loads(index.read_text())
    assert list(doc["files"]) == ["app/core.py"]
    assert doc["files"]["app/core.py"]["lines"] == {"1": [nodeid]}


def test_build_index_noop_when_no_contexts(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    index = _index_cache(monkeypatch, tmp_path)
    data = _FakeData(contexts=[], files=[], per_file={})
    covtool.build_index(_FakeCov(data))
    assert not index.exists()


def test_build_index_skips_files_outside_tree(tmp_path, monkeypatch):
    project = tmp_path / "project"
    project.mkdir()
    monkeypatch.chdir(project)
    index = _index_cache(monkeypatch, project)
    outside = tmp_path / "elsewhere.py"
    outside.write_text("x = 1\n")

    data = _FakeData(
        contexts=["c"],
        files=[str(outside)],
        per_file={str(outside): {1: ["t.py::a|run"]}},
    )
    covtool.build_index(_FakeCov(data))
    # only out-of-tree file -> nothing to index -> no file written
    assert not index.exists()


def test_build_index_skips_files_on_relpath_valueerror(tmp_path, monkeypatch):
    # os.path.relpath raises ValueError for a path on a different drive (Windows);
    # such a file isn't in the project tree and must be skipped, not crash.
    monkeypatch.chdir(tmp_path)
    index = _index_cache(monkeypatch, tmp_path)

    def _raise(*_args, **_kwargs):
        raise ValueError("path is on mount 'D:', start on mount 'C:'")

    monkeypatch.setattr(covtool.os.path, "relpath", _raise)
    data = _FakeData(
        contexts=["c"],
        files=["D:\\other\\thing.py"],
        per_file={"D:\\other\\thing.py": {1: ["t.py::a|run"]}},
    )
    covtool.build_index(_FakeCov(data))
    assert not index.exists()


def test_build_index_drops_vanished_source(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    index = _index_cache(monkeypatch, tmp_path)
    gone = tmp_path / "gone.py"  # never created -> _file_sha256 returns None

    data = _FakeData(
        contexts=["c"],
        files=[str(gone)],
        per_file={str(gone): {1: ["t.py::a|run"]}},
    )
    covtool.build_index(_FakeCov(data))
    assert not index.exists()


def test_build_index_skips_lines_with_only_empty_contexts(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    index = _index_cache(monkeypatch, tmp_path)
    src = tmp_path / "mod.py"
    src.write_text("a = 1\n")

    data = _FakeData(
        contexts=["c"],
        files=[str(src)],
        per_file={str(src): {1: [""]}},  # empty context only -> line dropped
    )
    covtool.build_index(_FakeCov(data))
    assert not index.exists()


def _measure(project, *, suffix=True, config_file=True, call_add=True):
    """Record real coverage data for `project` the way a pool worker does: a
    suffixed data file (or, with `suffix=False`, the plain file an in-process
    pytest-cov session leaves). calc.py ends up at 4/6 statements, full.py at
    3/3: 7/9 = 77.78% in total."""
    import coverage

    cov = coverage.Coverage(
        source=[str(project / "pkg")], data_suffix=suffix or None, config_file=config_file
    )
    cov.start()
    try:
        ns = runpy.run_path(str(project / "pkg" / "calc.py"))
        runpy.run_path(str(project / "pkg" / "full.py"))
        if call_add:
            ns["add"](1, 2)
    finally:
        cov.stop()
    cov.save()


def _project(tmp_path, monkeypatch, rc=None):
    (tmp_path / "pkg").mkdir()
    (tmp_path / "pkg" / "__init__.py").write_text("")
    # Three defs + three returns; only add() runs -> 4 of 6 statements.
    (tmp_path / "pkg" / "calc.py").write_text(
        "def add(a, b):\n    return a + b\n\n\n"
        "def sub(a, b):\n    return a - b\n\n\n"
        "def mul(a, b):\n    return a * b\n"
    )
    (tmp_path / "pkg" / "full.py").write_text("X = 1\nY = 2\nZ = 3\n")
    if rc is not None:
        (tmp_path / ".coveragerc").write_text(rc)
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _fail_lines(out):
    return out.count("FAIL Required test coverage")


def test_main_combines_worker_files_and_reports(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    assert covtool.main(["--cov-report=term"]) == 0
    out = capsys.readouterr().out
    assert "TOTAL" in out and "78%" in out
    # combined into the plain data file; the worker files are gone
    assert (p / ".coverage").exists()
    assert not list(p.glob(".coverage.*"))


def test_main_fail_under_gates_once_with_two_reports(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    status = covtool.main(["--cov-report=term", "--cov-report=xml", "--cov-fail-under=95"])
    out = capsys.readouterr().out
    assert status == 1
    assert _fail_lines(out) == 1
    assert "Coverage XML written to file coverage.xml" in out


def test_main_fail_under_gated_without_a_percentage_report(tmp_path, monkeypatch, capsys):
    # P5: no report (`--cov-report=`) or only annotate must still gate.
    for i, reports in enumerate((["--cov-report="], ["--cov-report=annotate"])):
        p = tmp_path / f"case{i}"
        p.mkdir()
        _project(p, monkeypatch)
        _measure(p)
        status = covtool.main([*reports, "--cov-fail-under=95"])
        out = capsys.readouterr().out
        assert status == 1, reports
        assert _fail_lines(out) == 1, reports
        assert "TOTAL" not in out


def test_main_met_threshold_reports_reached(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    assert covtool.main(["--cov-report=", "--cov-fail-under=50"]) == 0
    assert "Required test coverage of 50% reached" in capsys.readouterr().out


def test_main_fail_under_from_coveragerc(tmp_path, monkeypatch, capsys):
    # A11: `[report] fail_under` applies when no flag is passed.
    p = _project(tmp_path, monkeypatch, rc="[report]\nfail_under = 95\nshow_missing = True\n")
    _measure(p)
    status = covtool.main(["--cov-report=term"])
    out = capsys.readouterr().out
    assert status == 1
    assert _fail_lines(out) == 1
    # show_missing from the config is honoured for plain `term`
    assert "Missing" in out


def test_main_fail_under_uses_report_precision(tmp_path, monkeypatch, capsys):
    # A11: pytest-cov gates on the total rounded to the report precision
    # (coverage's should_fail_under): 77.78% passes 78 at precision 0 ...
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    assert covtool.main(["--cov-report=", "--cov-fail-under=78"]) == 0
    capsys.readouterr()
    # ... but not at precision 2, where 77.78 < 78.
    _measure(p)
    assert covtool.main(["--cov-report=", "--cov-fail-under=78", "--cov-precision=2"]) == 1
    assert "total of 77.78 is less than fail-under=78.00" in capsys.readouterr().out


def test_main_cov_config_data_file(tmp_path, monkeypatch, capsys):
    # B3: --cov-config decides where the data lives.
    p = _project(tmp_path, monkeypatch)
    (p / "cov.cfg").write_text("[run]\ndata_file = covdata/.coverage\n")
    (p / "covdata").mkdir()
    _measure(p, config_file=str(p / "cov.cfg"))
    status = covtool.main(["--cov-config=cov.cfg", "--cov-report=term", "--cov-fail-under=95"])
    out = capsys.readouterr().out
    assert status == 1
    assert "No data to report" not in out
    assert "TOTAL" in out
    assert not list((p / "covdata").glob(".coverage.*"))


def test_main_skip_covered_modifier(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    covtool.main(["--cov-report=term-missing:skip-covered"])
    out = capsys.readouterr().out
    assert "pkg/calc.py" in out
    assert "pkg/full.py" not in out


def test_main_no_data_reports_zero_and_gates(tmp_path, monkeypatch, capsys):
    # Every test skipped/deselected: no data files. Report 0% (no traceback)
    # and gate like pytest-cov.
    _project(tmp_path, monkeypatch)
    assert covtool.main(["--cov-report=term"]) == 0
    capsys.readouterr()
    assert covtool.main(["--cov-report=term", "--cov-fail-under=80"]) == 1
    out = capsys.readouterr().out
    assert "Failed to generate report" in out
    assert "Total coverage: 0.00%" in out


def test_main_already_reported_prints_nothing(tmp_path, monkeypatch, capsys):
    # S5: in-process pytest-cov already reported and gated the session.
    p = _project(tmp_path, monkeypatch)
    _measure(p, suffix=False)
    status = covtool.main(["--cov-report=term", "--cov-fail-under=95", "--rstest-cov-reported"])
    assert status == 0
    assert capsys.readouterr().out == ""


def test_main_already_reported_still_builds_index(tmp_path, monkeypatch):
    p = _project(tmp_path, monkeypatch)
    _measure(p, suffix=False)
    seen = {}
    monkeypatch.setattr(covtool, "build_index", lambda cov: seen.setdefault("index", cov))
    covtool.main(["--cov-context=test", "--cov-report=", "--rstest-cov-reported"])
    assert "index" in seen


def test_main_html_and_json_written(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    covtool.main(["--cov-report=html:cov", "--cov-report=json", "--cov-context=test"])
    out = capsys.readouterr().out
    assert "Coverage HTML written to dir cov" in out
    assert "Coverage JSON written to file coverage.json" in out
    assert (p / "cov" / "index.html").exists()
    assert (p / "coverage.json").exists()


def test_main_lcov_report(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    covtool.main(["--cov-report=lcov:cov.info"])
    assert (p / "cov.info").exists()
    assert "Coverage LCOV written to file cov.info" in capsys.readouterr().out


def test_main_unknown_report_kind_is_skipped(tmp_path, monkeypatch, capsys):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    assert covtool.main(["--cov-report=bogus"]) == 0
    assert "TOTAL" not in capsys.readouterr().out


def test_main_index_build_failure_does_not_fail_run(tmp_path, monkeypatch):
    p = _project(tmp_path, monkeypatch)
    _measure(p)

    def boom(cov):
        raise RuntimeError("index broke")

    monkeypatch.setattr(covtool, "build_index", boom)
    assert covtool.main(["--cov-report=term", "--cov-context=test"]) == 0


def test_main_runs_diff_coverage_when_flags_present(tmp_path, monkeypatch):
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    seen = {}

    def fake_diff(cov, diff_lines, diff_out):
        seen["args"] = (diff_lines, diff_out)

    monkeypatch.setattr(covtool, "diff_coverage", fake_diff)
    status = covtool.main(
        ["--cov-report=term", "--rstest-diff-lines=lines.json", "--rstest-diff-out=out.json"]
    )
    assert status == 0
    assert seen["args"] == ("lines.json", "out.json")


def test_main_diff_coverage_failure_does_not_fail_run(tmp_path, monkeypatch):
    p = _project(tmp_path, monkeypatch)
    _measure(p)

    def boom(cov, diff_lines, diff_out):
        raise RuntimeError("diff broke")

    monkeypatch.setattr(covtool, "diff_coverage", boom)
    assert (
        covtool.main(
            ["--cov-report=term", "--rstest-diff-lines=lines.json", "--rstest-diff-out=out.json"]
        )
        == 0
    )


def test_main_no_cov_does_nothing(tmp_path, monkeypatch, capsys):
    # --no-cov disables pytest-cov: no report, no gate, no "No data" warning.
    p = _project(tmp_path, monkeypatch)
    _measure(p)
    assert covtool.main(["--no-cov", "--cov-report=term", "--cov-fail-under=95"]) == 0
    assert capsys.readouterr().out == ""
