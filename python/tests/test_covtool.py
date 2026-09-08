"""Unit tests for the coverage-combine tool's arg parsing and index helpers."""

import json
import types

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


class _FakeCoverageModule(types.ModuleType):
    """A fake `coverage` module injected into sys.modules for main()."""

    class CoverageException(Exception):
        pass

    def __init__(self, report_pct=100.0, raise_on_report=False):
        super().__init__("coverage")
        self._report_pct = report_pct
        self._raise_on_report = raise_on_report
        self.calls = []
        module = self

        class Coverage:
            def combine(self, keep=False):
                module.calls.append(("combine", keep))

            def save(self):
                module.calls.append(("save",))

            def load(self):
                module.calls.append(("load",))

            def get_data(self):
                return _FakeData([], [], {})

            def report(self, show_missing=False):
                module.calls.append(("report", show_missing))
                if module._raise_on_report:
                    raise module.CoverageException("boom")
                return module._report_pct

            def xml_report(self, outfile=None):
                module.calls.append(("xml", outfile))
                return module._report_pct

            def html_report(self, directory=None, show_contexts=False):
                module.calls.append(("html", directory, show_contexts))
                return module._report_pct

            def json_report(self, outfile=None, show_contexts=False):
                module.calls.append(("json", outfile, show_contexts))
                return module._report_pct

            def lcov_report(self, outfile=None):
                module.calls.append(("lcov", outfile))
                return module._report_pct

            def annotate(self, directory=None):
                module.calls.append(("annotate", directory))

        self.Coverage = Coverage


def _install_fake_coverage(monkeypatch, **kwargs):
    fake = _FakeCoverageModule(**kwargs)
    monkeypatch.setitem(__import__("sys").modules, "coverage", fake)
    return fake


def test_main_combines_saves_loads_and_reports(monkeypatch):
    fake = _install_fake_coverage(monkeypatch)
    status = covtool.main(["--cov-report=term"])
    assert status == 0
    assert ("combine", False) in fake.calls
    assert ("save",) in fake.calls
    assert ("load",) in fake.calls
    assert ("report", False) in fake.calls


def test_main_term_missing_sets_show_missing(monkeypatch):
    fake = _install_fake_coverage(monkeypatch)
    covtool.main(["--cov-report=term-missing"])
    assert ("report", True) in fake.calls


def test_main_fail_under_returns_1(monkeypatch, capsys):
    _install_fake_coverage(monkeypatch, report_pct=50.0)
    status = covtool.main(["--cov-fail-under=80"])
    assert status == 1
    assert "not reached" in capsys.readouterr().out


def test_main_fail_under_met_returns_0(monkeypatch):
    _install_fake_coverage(monkeypatch, report_pct=95.0)
    assert covtool.main(["--cov-fail-under=80"]) == 0


def test_main_coverage_exception_returns_1(monkeypatch):
    _install_fake_coverage(monkeypatch, raise_on_report=True)
    assert covtool.main(["--cov-report=term"]) == 1


def test_main_unknown_report_kind_is_skipped(monkeypatch, caplog):
    fake = _install_fake_coverage(monkeypatch)
    status = covtool.main(["--cov-report=bogus"])
    assert status == 0
    # no report method invoked for an unknown kind
    assert not any(c[0] in ("report", "xml", "html", "json") for c in fake.calls)


def test_main_html_uses_context_mode(monkeypatch, capsys):
    fake = _install_fake_coverage(monkeypatch)
    covtool.main(["--cov-report=html:cov", "--cov-context=test"])
    assert ("html", "cov", True) in fake.calls
    assert "Coverage HTML written" in capsys.readouterr().out


def test_main_xml_report_prints_path(monkeypatch, capsys):
    fake = _install_fake_coverage(monkeypatch)
    covtool.main(["--cov-report=xml:cov.xml"])
    assert ("xml", "cov.xml") in fake.calls
    assert "Coverage XML written" in capsys.readouterr().out


def test_main_json_report_passes_context_flag(monkeypatch):
    fake = _install_fake_coverage(monkeypatch)
    covtool.main(["--cov-report=json", "--cov-context=test"])
    assert ("json", None, True) in fake.calls


def test_main_lcov_report(monkeypatch):
    fake = _install_fake_coverage(monkeypatch)
    covtool.main(["--cov-report=lcov:cov.info"])
    assert ("lcov", "cov.info") in fake.calls


def test_main_annotate_report(monkeypatch):
    fake = _install_fake_coverage(monkeypatch)
    status = covtool.main(["--cov-report=annotate"])
    assert status == 0  # annotate yields no pct, never trips fail-under
    assert ("annotate", None) in fake.calls


def test_main_context_mode_builds_index(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)
    _install_fake_coverage(monkeypatch)
    called = {}
    monkeypatch.setattr(covtool, "build_index", lambda cov: called.setdefault("hit", True))
    covtool.main(["--cov-report=term", "--cov-context=test"])
    assert called.get("hit") is True


def test_main_index_build_failure_does_not_fail_run(monkeypatch):
    _install_fake_coverage(monkeypatch)

    def boom(cov):
        raise RuntimeError("index broke")

    monkeypatch.setattr(covtool, "build_index", boom)
    # build_index blows up but the run still returns its report status
    assert covtool.main(["--cov-report=term", "--cov-context=test"]) == 0
