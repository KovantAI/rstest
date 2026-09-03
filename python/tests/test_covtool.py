"""Unit tests for the coverage-combine tool's arg parsing and index helpers."""

from rstest_worker import covtool


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
