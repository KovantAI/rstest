Feature: Daily local developer
  An engineer in the edit-run-fix loop: runs a subset, reads the traceback,
  reruns the failures with --lf, drops a breakpoint() or uses --pdb, Ctrl-Cs a
  slow run, leaves --watch running and uses --changed before pushing.
  Interactive parts (pty, signals, watch) run under hard timeouts and always
  kill the process group, so a hang fails a step instead of hanging the run.
  The "pytest oracle" is plain pytest from the worker venv, run on the same
  files: where rstest claims parity, its answer is the expected one.

  Rule: every selection flavour picks what pytest picks

    Background:
      # Unescaped ids, as a project with non-ASCII parametrize ids configures.
      Given a file "pytest.ini" containing:
        """
        [pytest]
        markers =
            slow: slow
        disable_test_id_escaping_and_forfeit_all_rights_to_community_support = true
        """
      And a file "tests/test_sel.py" containing:
        """
        import pytest

        class TestCls:
            def test_a(self): pass

            @pytest.mark.slow
            def test_b(self): pass

        @pytest.mark.parametrize('v', ['a::b', 'x[y', 'with space', 'ünï'], ids=str)
        def test_p(v): pass

        @pytest.mark.slow
        def test_m(): pass
        """
      And a file "tests/test_other.py" containing:
        """
        def test_other(): pass
        """
      And a file "tests/sub/test_sub.py" containing:
        """
        def test_sub(): pass
        """
      And a file "tests/sub/test_glob_x.py" containing:
        """
        def test_glob(): pass
        """
      And a file "tests/ign/test_ign.py" containing:
        """
        def test_ign(): pass
        """

    Scenario Outline: DV-01 selecting by <label> runs what pytest --co selects, at -n 0 and -n 2
      When the pytest oracle runs "pytest --co -q -p no:cacheprovider <selection>"
      Then the pytest oracle exits 0 and collects at least 1 test
      When I run "rstest -n 0 -q --report-json dv-sel-0.json <selection>"
      Then the run succeeds
      And the JSON report "dv-sel-0.json" lists exactly the tests the pytest oracle collected
      When I run "rstest -n 2 -q --report-json dv-sel-2.json <selection>"
      Then the run succeeds
      And the JSON report "dv-sel-2.json" lists exactly the tests the pytest oracle collected

      Examples:
        | label                     | selection                                        |
        | -k                        | -k 'TestCls or space'                            |
        | -m                        | -m slow                                          |
        | -m not                    | -m 'not slow'                                    |
        | file::Class::test         | tests/test_sel.py::TestCls::test_b               |
        | file::test[id with ::]    | 'tests/test_sel.py::test_p[a::b]'                |
        | file::test[id with []     | 'tests/test_sel.py::test_p[x[y]'                 |
        | file::test[id with space] | 'tests/test_sel.py::test_p[with space]'          |
        | file::test[unicode id]    | 'tests/test_sel.py::test_p[ünï]'                 |
        | directory                 | tests/sub                                        |
        | --deselect                | --deselect tests/test_sel.py::TestCls::test_a    |
        | --ignore                  | --ignore tests/ign                               |
        | --ignore-glob             | --ignore-glob '*_glob_*'                         |

  Rule: -s in the pool shows live output

    Scenario: DV-12 -n 2 -s prints every test's output, with one capture-mode notice
      # rstest runs an -s session in one process (pytest's own output) and says so once.
      Given 3 files "test_p{i}.py" each containing:
        """
        def test_p{i}():
            print('dv-live-print-{i}')
        """
      When I run "rstest -n 2"
      Then the run succeeds
      And stdout does not contain "dv-live-print"
      When I run "rstest -n 2 -s"
      Then the run succeeds
      And stdout contains "dv-live-print-0"
      And stdout contains "dv-live-print-1"
      And stdout contains "dv-live-print-2"
      And the output contains "-s runs the session" exactly once

  Rule: rerun history matches pytest's

    Scenario: DV-02 --lf, --ff, --nf and a subset run keep lastfailed in step with pytest
      # The same sequence runs under the pytest oracle (in a twin copy of the
      # project) and under rstest -n 2; after each step lastfailed must match.
      # The conftest logs each test's start, so dispatch order is observable.
      Given 5 files "tests/test_m{i}.py" each containing:
        """
        import os

        def test_{i}_0():
            assert not os.path.exists('fail_{i}_0')

        def test_{i}_1():
            assert not os.path.exists('fail_{i}_1')
        """
      And a file "tests/conftest.py" containing:
        """
        import os, time, pytest

        @pytest.fixture(autouse=True)
        def _dv_order_log(request):
            root = str(request.config.rootpath)
            with open(os.path.join(root, f'order.{os.getpid()}'), 'a') as f:
                w = os.environ.get('PYTEST_XDIST_WORKER', '-')
                f.write(f'{time.time():.6f} {w} {request.node.nodeid}\n')
            yield
        """
      And an empty file "fail_1_0"
      And an empty file "fail_4_1"
      And a pytest oracle twin of the project
      When I run "rstest -n 2 -q --report-json dv-lf-1.json" and the pytest oracle runs "pytest -q -p no:randomly" in the twin
      Then the exit code is 1
      And stdout contains "2 failed, 8 passed"
      And the first test the order log recorded is not "tests/test_m4.py::test_4_1"
      And the pytest oracle twin wrote a lastfailed file
      And lastfailed matches the pytest oracle twin's
      When I delete "fail_1_0" from the project and its twin
      And I run "rstest -n 2 -q --report-json dv-lf-lf.json --lf" and the pytest oracle runs "pytest -q -p no:randomly --lf" in the twin
      Then the JSON report "dv-lf-lf.json" lists exactly "tests/test_m1.py::test_1_0" and "tests/test_m4.py::test_4_1"
      And stdout contains "1 failed, 1 passed"
      And lastfailed matches the pytest oracle twin's
      When I run "rstest -n 2 -q --report-json dv-lf-ff.json --ff" and the pytest oracle runs "pytest -q -p no:randomly --ff" in the twin
      # Dispatch order, not start time (see --nf below).
      Then some worker's replay-journal assignment starts with "tests/test_m4.py::test_4_1"
      And stdout contains "1 failed, 9 passed"
      And lastfailed matches the pytest oracle twin's
      When I add "tests/test_new.py" to the project and its twin, containing:
        """
        def test_new():
            pass
        """
      And I run "rstest -n 2 -q --report-json dv-lf-nf.json --nf" and the pytest oracle runs "pytest -q -p no:randomly --nf" in the twin
      # Dispatch order, not start time: both workers' first items start within
      # microseconds of each other. The replay journal records each worker's
      # assignment order; the head of the --nf order leads its worker's list.
      Then some worker's replay-journal assignment starts with "tests/test_new.py::test_new"
      And lastfailed matches the pytest oracle twin's
      # A passing subset run must keep the other file's failure.
      When I run "rstest -n 2 -q --report-json dv-lf-sub.json tests/test_m0.py" and the pytest oracle runs "pytest -q -p no:randomly tests/test_m0.py" in the twin
      Then the run succeeds
      And the pytest oracle twin's lastfailed lists "tests/test_m4.py::test_4_1"
      And lastfailed matches the pytest oracle twin's

  Rule: a run leaves a committed tree as it found it

    Background:
      Given a file ".gitignore" containing:
        """
        __pycache__/
        """
      And a file "pyproject.toml" containing:
        """
        [tool.pytest.ini_options]
        testpaths = ['tests']
        """
      And a file "tests/test_a.py" containing:
        """
        def test_a():
            pass

        def test_b():
            assert 0
        """
      And a file "tests/test_c.py" containing:
        """
        def test_c():
            pass
        """
      And the project is committed to a fresh git repository

    Scenario: DV-11 pytest runs from the root and from tests/ leave the tree clean
      When the pytest oracle runs "pytest -q"
      And the pytest oracle runs "pytest -q" in "tests"
      Then git status --porcelain -uall shows a clean tree
      And ".pytest_cache" is a directory

    Scenario: DV-11 rstest leaves the tree clean and writes lastfailed to the rootdir cache
      When I run "rstest -n 2"
      Then the exit code is 1
      And stdout contains "1 failed, 2 passed"
      And git status --porcelain -uall shows a clean tree
      When I remove ".pytest_cache/v/cache/lastfailed" if present
      And I run "rstest -n 2" in "tests"
      Then ".pytest_cache/v/cache/lastfailed" is a file
      And "tests/.pytest_cache" does not exist

  Rule: --incremental shares one rootdir cache across run locations

    Scenario: DV-13 --incremental from the root, then tests/unit, then the root again skips what the first run recorded
      # Cache records must be rootdir-relative, so the subdirectory run neither
      # poisons nor erases what the root run recorded.
      Given a file "pyproject.toml" containing:
        """
        [tool.pytest.ini_options]
        testpaths = ['tests']
        pythonpath = ['.']
        """
      And an empty file "app/__init__.py"
      And a file "app/core.py" containing:
        """
        def one():
            return 1
        """
      And a file "app/util.py" containing:
        """
        def two():
            return 2
        """
      And a file "tests/test_top.py" containing:
        """
        from app import core

        def test_t1():
            assert core.one() == 1

        def test_t2():
            assert core.one() + 1 == 2
        """
      And a file "tests/unit/test_u.py" containing:
        """
        from app import util

        def test_u1():
            assert util.two() == 2

        def test_u2():
            assert util.two() * 2 == 4
        """
      When I run "rstest -n 2 --cov=app --cov-context=test --cov-report= --incremental" after clearing stale .coverage files
      Then the run succeeds
      And stdout contains "4 passed"
      And the incremental outcomes record 4 green tests
      # Touch the unit tests' dependency so the subdir run really executes them
      # and rewrites the coverage index from tests/unit/.
      Given a file "app/util.py" containing:
        """
        def two():
            return 2  # touched
        """
      When I run "rstest -n 2 --cov=app --cov-context=test --cov-report= --incremental" in "tests/unit" after clearing stale .coverage files
      Then the run succeeds
      And stdout contains "2 passed"
      And stdout does not contain "cached"
      And every key in the coverage index names a file under the project root
      When I run "rstest -n 2 --cov=app --cov-context=test --cov-report= --incremental" after clearing stale .coverage files
      Then the run succeeds
      And stderr contains "4 of 4 test(s) unchanged"
      And stdout contains "(4 cached)"

    Scenario: DV-14 a run from a subdirectory keeps its cache at the rootdir, as documented
      Given a file "pyproject.toml" containing:
        """
        [tool.pytest.ini_options]
        testpaths = ['tests']
        """
      And a file "tests/unit/test_u.py" containing:
        """
        def test_u():
            pass
        """
      When I run "rstest -n 2" in "tests/unit"
      Then the run succeeds
      And ".rstest_cache" is a directory
      And "tests/unit/.rstest_cache" does not exist
      Given the table row of "docs/reference/environment.md" starting with "| `RSTEST_CACHE`"
      Then that docs section contains "default `.rstest_cache` at the pytest rootdir"
      And that docs section does not contain "invocation directory"

  Rule: --changed and --watch follow the import graph through conftest.py

    Background:
      # conftest.py imports app.other.mul for a fixture.
      Given an empty file "app/__init__.py"
      And a file "app/other.py" containing:
        """
        def mul(a, b):
            return a * b
        """
      And a file "tests/conftest.py" containing:
        """
        import pytest
        from app.other import mul

        @pytest.fixture
        def doubled():
            return mul(2, 3)
        """
      And a file "tests/test_calc.py" containing:
        """
        def test_calc(doubled):
            assert doubled == 6
        """
      And a file "tests/test_other.py" containing:
        """
        from app.other import mul

        def test_other():
            assert mul(2, 3) == 6
        """
      And a file "tests/test_plain.py" containing:
        """
        def test_plain():
            assert True
        """
      And a file "pyproject.toml" containing:
        """
        [tool.pytest.ini_options]
        testpaths = ['tests']
        pythonpath = ['.']
        """

    Scenario: DV-03 --changed runs a changed module's importers, including a conftest importer's subtree
      Given the project is committed to a fresh git repository
      # A different byte length from the original, so a same-second rewrite can
      # never be served from a stale .pyc (Python keys pycs on mtime + size).
      And a file "app/other.py" containing:
        """
        def mul(a, b):
            return a * b + 1
        """
      When I run "rstest -n 2"
      Then stdout contains "2 failed, 1 passed"
      When I run "rstest --changed -n 2 -v"
      Then stdout contains "test_other.py::test_other FAILED"
      And stdout contains "test_calc.py::test_calc FAILED"
      And stdout contains "2 failed"
      When I run "rstest --changed-strict -n 2"
      Then stdout contains "2 failed"

    Scenario: DV-04 --watch reruns the right tests for each kind of edit
      # Each edit's output is collected up to the watcher's next idle prompt.
      Given a file "tests/data.txt" containing:
        """
        x
        """
      When I start "rstest --watch -n 2 -v" and wait up to 60 seconds for its idle prompt
      Then the watcher printed its idle prompt
      And the output contains "3 passed"
      # (b) a source edit
      When I save "app/other.py" while watching:
        """
        def mul(a, b):
            return a * b + 1
        """
      Then the watcher printed its idle prompt
      And the output contains "test_other.py::test_other FAILED"
      And the output contains "test_calc.py::test_calc FAILED"
      And the output contains "2 failed"
      When I save "app/other.py" while watching:
        """
        def mul(a, b):
            return a * b
        """
      Then the watcher printed its idle prompt
      And the output does not contain "failed"
      # (c) a new test file
      When I save "tests/test_new.py" while watching:
        """
        def test_new():
            assert True
        """
      Then the watcher printed its idle prompt
      And the output contains "test_new.py::test_new PASSED"
      And the output contains "1 passed"
      # (d) a deleted test file
      When I delete "tests/test_new.py" while watching
      Then the watcher printed its idle prompt
      And the output shows no "Traceback" and no "error" in any case
      And the watcher is still running
      # (e) a syntax error, then its fix
      When I save "tests/test_plain.py" while watching:
        """
        def test_plain(:
        """
      Then the watcher printed its idle prompt
      And the output contains "SyntaxError"
      And the watcher is still running
      When I save "tests/test_plain.py" while watching:
        """
        def test_plain():
            assert 1 == 1
        """
      Then the watcher printed its idle prompt
      And the output contains "test_plain.py::test_plain PASSED"
      And the output contains "1 passed"
      # (f) a data file edit
      When I save "tests/data.txt" while watching and give it 4 seconds to start a rerun:
        """
        changed data
        """
      Then the watcher did not start a rerun
      And the watcher is still running

  Rule: a debugger works in the pool

    Background:
      Given a file "tests/test_bp.py" containing:
        """
        def test_bp():
            x = 1
            breakpoint()
            assert x == 1

        def test_after_bp():
            pass
        """
      And a file "tests/test_more.py" containing:
        """
        def test_more():
            pass
        """
      And a file "tests/test_pdb.py" containing:
        """
        def test_fail():
            assert 1 == 2

        def test_ok():
            pass
        """

    @posix_only
    Scenario: DV-05 single-worker mode is pytest itself and stops at breakpoint()
      When I run "rstest -n 0 tests/test_bp.py" on a pty, typing "c" at the (Pdb) prompt
      Then the exit code is 0
      And the output contains "(Pdb)"
      And the output contains "2 passed"

    @posix_only
    Scenario: DV-05 breakpoint() in the pool ends with a prompt or a hint and accounts for every test
      When I run "rstest -n 2 --report-json dv-bp.json tests/test_bp.py" on a pty, typing "c" at the (Pdb) prompt
      Then the pty run ended before its timeout
      And the output outside the rstest banner shows a (Pdb) prompt or a hint naming -n 0 or -s
      And the JSON report "dv-bp.json" lists 2 tests, including "tests/test_bp.py::test_after_bp"

    @posix_only
    Scenario: DV-06 --pdb in the pool gives a real prompt and 'q' exits 2 (interrupted)
      When I run "rstest -n 2 --pdb tests/test_pdb.py" on a pty, typing "q" at the (Pdb) prompt
      Then the exit code is 2
      And the output contains "(Pdb)"

  Rule: tracebacks read like pytest's in every --tb style

    Background:
      Given a file "test_tb.py" containing:
        """
        def helper(v):
            total = v + 1
            assert total == 0, 'helper says no'


        def test_multi():
            data = [1, 2, 3]
            print('dv-captured-marker')
            helper(len(data))
        """

    Scenario Outline: DV-07 --tb=<style> failure body matches pytest's
      When the pytest oracle runs "pytest -q -p no:cacheprovider --tb=<style>"
      And I run "rstest -n 2 -q -p no:cacheprovider --tb=<style>"
      Then the pytest oracle's failure body for "test_multi" has more than 5 lines
      And rstest's failure body for "test_multi" matches the pytest oracle's after the first line
      And the failure body for "test_multi" starts with "    def test_multi():" under both rstest and the pytest oracle

      Examples:
        | style |
        | auto  |
        | long  |

    Scenario: DV-07 --tb=short failure body matches pytest's
      When the pytest oracle runs "pytest -q -p no:cacheprovider --tb=short"
      And I run "rstest -n 2 -q -p no:cacheprovider --tb=short"
      Then the pytest oracle's failure body for "test_multi" has more than 3 lines
      And rstest's failure body for "test_multi" matches the pytest oracle's exactly

    Scenario: DV-07 --tb=native traceback ends like pytest's (test frames + exception)
      When the pytest oracle runs "pytest -q -p no:cacheprovider --tb=native"
      And I run "rstest -n 2 -q -p no:cacheprovider --tb=native"
      Then the pytest oracle's failure body for "test_multi" has more than 4 lines
      And the last 4 lines of rstest's failure body for "test_multi" match the pytest oracle's
      And rstest's failure body for "test_multi" starts with "Traceback (most recent call last):"

    Scenario: DV-07 --tb=line prints one path:line: msg line per failure
      When I run "rstest -n 2 -q -p no:cacheprovider --tb=line"
      Then stdout matches "test_tb\.py:3: AssertionError: helper says no"

    Scenario: DV-07 --tb=no reports the failure without a failure block or captured output
      When I run "rstest -n 2 -q -p no:cacheprovider --tb=no"
      Then stdout contains "1 failed"
      And stdout does not contain "--- FAILED"
      And stdout does not contain "dv-captured-marker"

  Rule: terminal output respects the terminal

    Background:
      Given 4 files "test_t{i}.py" each containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(3))
        def test_a_rather_long_test_name_to_force_wrapping_{i}(i):
            time.sleep(0.15)

        def test_fail_{i}():
            assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}
        """

    Scenario: DV-10 (d) a piped FORCE_COLOR=1 run is colored all or nothing
      When I run "rstest -n 2" with "FORCE_COLOR=1"
      Then the exit code is 1
      And the last line of stdout matches "4 failed"
      And if stdout has any escape sequence, its last line has one too

    @posix_only
    Scenario: DV-10 a plain pty run is colored (so the escape checks are not vacuous)
      When I run "rstest -n 2" on a pty
      Then the exit code is 1
      And the output contains ANSI escape sequences

    @posix_only
    Scenario: DV-10 (a) on a 40-column pty the live footer fits in 40 columns
      When I run "rstest -n 2" on a 40-column pty
      Then the exit code is 1
      And the live footer was drawn
      And every live footer line fits in 40 columns

    @posix_only
    Scenario Outline: DV-10 <case> on a pty prints zero escape sequences
      When I run "<command>" on a pty with "<env>"
      Then the exit code is 1
      And the output contains no escape character

      Examples:
        | case           | command                 | env                  |
        | (b) TERM=dumb  | rstest -n 2             | TERM=dumb            |
        | (c) NO_COLOR=1 | rstest -n 2             | NO_COLOR=1           |
        | (e) --color=no | rstest -n 2 --color=no  | TERM=xterm-256color  |

  Rule: stopping a run early is clean

    Scenario: DV-09 -x in the pool starts no new test once the first failure is reported
      # test_f[0] fails at once, the rest sleep; each test logs its start.
      Given a file "test_x.py" containing:
        """
        import os, time, pytest
        LOG = os.path.join(os.path.dirname(__file__), 'starts')

        def _log(kind, i):
            with open(f'{LOG}.{os.getpid()}', 'a') as f:
                f.write(f'{time.time():.6f} {kind} {i}\n')

        @pytest.mark.parametrize('i', range(8))
        def test_f(i):
            _log('start', i)
            if i == 0:
                _log('fail', i)
                assert False, 'boom'
            time.sleep(0.3)
        """
      When I run "rstest -n 2 -x -v"
      Then the exit code is 1
      And stdout contains "1 failed"
      And the start log recorded the failure
      And no test started more than 0.1 seconds after the failure was logged
      And the output contains "stopping after 1 failures"

    @posix_only
    Scenario: DV-08 Ctrl-C mid-run exits 2, says what did not run, and records nothing for in-flight tests
      Given a file "test_slow.py" containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(40))
        def test_slow(i):
            time.sleep(0.5)
        """
      When I run "rstest -n 4" and send SIGINT to its process group after 2.5 seconds
      Then the exit code is 2
      And the output contains "interrupted"
      And the output lists tests in flight on workers
      And rstest exited within 30 seconds of the SIGINT
      And the output matches "\b\d+ (tests? )?(not run|did not run|not started|unrun)"
      And lastfailed lists none of the in-flight tests
      And flakes.json has no entry for any in-flight test
      And durations.json has no 0.0 duration for any in-flight test
