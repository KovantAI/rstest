Feature: pytest / pytest-xdist migrator
  A team lead swaps `pytest -n auto` for `rstest` on an existing, configured
  suite (real addopts, conftest hooks, plugins), runs the readiness tools, and
  expects every result to stay identical. The worker venv has no
  pytest-xdist, so the oracle is plain `python -m pytest` from that venv, or
  the documented xdist behaviour.

  Fixture event log: "the fixture event logger" is a conftest.py defining
  `_log(**fields)`, which records one event per call, tagged with the worker
  id (`w`, "main" outside a pool), the pid and a timestamp (`t`). Tests import
  it with `from conftest import _log`.

  Rule: rstest gives the same outcomes as pytest on a configured suite

    # Each suite: plain pytest is the oracle; rstest at -n 0 and at -n 2 must
    # match its exit code, summary counts and per-test JUnit outcomes. pytest
    # writes an empty JUnit file on a usage error and rstest none: both mean
    # "no per-test outcomes".

    Scenario: MG-01 custom python_files / python_classes / python_functions
      Given a file "pytest.ini" containing:
        """
        [pytest]
        python_files = check_*.py
        python_classes = Suite*
        python_functions = verify_*
        """
      And a file "check_a.py" containing:
        """
        def verify_one():
            pass

        def verify_two():
            assert 0

        def test_not_collected():
            assert 0
        """
      And a file "check_b.py" containing:
        """
        class SuiteX:
            def verify_m(self):
                pass

        class TestIgnored:
            def verify_n(self):
                assert 0
        """
      And a file "test_not_matched.py" containing:
        """
        def verify_x():
            assert 0
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 strict config: strict markers, warnings as errors, strict xfail
      Given a file "pyproject.toml" containing:
        """
        [tool.pytest.ini_options]
        addopts = "--strict-markers"
        filterwarnings = ["error"]
        xfail_strict = true
        markers = ["slow: slow tests"]
        """
      And a file "test_s.py" containing:
        """
        import warnings, pytest

        @pytest.mark.slow
        def test_marked():
            pass

        def test_warns():
            warnings.warn('old', DeprecationWarning)

        @pytest.mark.xfail
        def test_xpass_strict():
            pass

        @pytest.mark.xfail
        def test_xfail():
            assert 0
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 --strict-markers in addopts with an unregistered mark
      Given a file "pytest.ini" containing:
        """
        [pytest]
        addopts = --strict-markers
        markers =
            slow: slow
        """
      And a file "test_ok.py" containing:
        """
        import pytest

        @pytest.mark.slow
        def test_ok():
            pass
        """
      And a file "test_unknown_mark.py" containing:
        """
        import pytest

        @pytest.mark.typo
        def test_t():
            pass
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 a missing required_plugins entry
      Given a file "pytest.ini" containing:
        """
        [pytest]
        required_plugins = pytest-mg-nonexistent
        """
      And a file "test_r.py" containing:
        """
        def test_r():
            pass
        """
      When I run plain pytest with JUnit output
      Then plain pytest exited with code 4
      When I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 an unsatisfiable minversion
      Given a file "pytest.ini" containing:
        """
        [pytest]
        minversion = 99.0
        """
      And a file "test_r.py" containing:
        """
        def test_r():
            pass
        """
      When I run plain pytest with JUnit output
      Then plain pytest exited with code 4
      When I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 --import-mode=importlib with duplicate basenames
      Given a file "pytest.ini" containing:
        """
        [pytest]
        addopts = --import-mode=importlib
        """
      And a file "a/test_same.py" containing:
        """
        def test_x():
            pass
        """
      And a file "b/test_same.py" containing:
        """
        def test_x():
            assert 0
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 a modifyitems hook that adds skip and strict xfail marks
      Given a file "conftest.py" containing:
        """
        import pytest

        def pytest_collection_modifyitems(config, items):
            for it in items:
                if 'skipme' in it.name:
                    it.add_marker(pytest.mark.skip(reason='hook'))
                if 'xfailme' in it.name:
                    it.add_marker(pytest.mark.xfail(reason='hook', strict=True))
        """
      And a file "test_h.py" containing:
        """
        def test_plain():
            pass

        def test_skipme():
            assert 0

        def test_xfailme():
            assert 0
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 a modifyitems hook that deselects tests
      Given a file "conftest.py" containing:
        """
        def pytest_collection_modifyitems(config, items):
            drop = [it for it in items if 'deselectme' in it.name]
            config.hook.pytest_deselected(items=drop)
            items[:] = [it for it in items if it not in drop]
        """
      And a file "test_d.py" containing:
        """
        def test_plain():
            pass

        def test_deselectme():
            assert 0
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 parametrize ids with separators, brackets, spaces and unicode
      Given a file "test_ids.py" containing:
        """
        import pytest

        @pytest.mark.parametrize(
            'v', ['a::b', 'x[1]', 'p/q', 'with space', 'ünïcöde']
        )
        def test_v(v):
            assert v != 'p/q'

        @pytest.mark.parametrize('v', [1, 2], ids=['id::colon', 'id [bracket]'])
        def test_ids(v):
            pass
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 doctests from modules and text files via addopts
      Given a file "pytest.ini" containing:
        """
        [pytest]
        addopts = --doctest-modules --doctest-glob=*.txt
        """
      And a file "mod.py" containing:
        """
        def add(a, b):
            '''
            >>> add(1, 2)
            3
            '''
            return a + b

        def bad():
            '''
            >>> bad()
            1
            '''
            return 2
        """
      And a file "doc.txt" containing:
        """
        >>> 1 + 1
        2
        """
      And a file "test_plain.py" containing:
        """
        def test_p():
            pass
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 every flavour of skip and xfail
      Given a file "test_sx.py" containing:
        """
        import sys, pytest

        @pytest.mark.skip(reason='r')
        def test_skip():
            pass

        @pytest.mark.skipif(sys.version_info > (3,), reason='py3')
        def test_skipif():
            pass

        @pytest.mark.xfail(reason='x')
        def test_xfail():
            assert 0

        @pytest.mark.xfail(strict=True)
        def test_xpass_strict():
            pass

        @pytest.mark.xfail
        def test_xpass():
            pass

        def test_imperative_skip():
            pytest.skip('now')
        """
      And a file "test_modskip.py" containing:
        """
        import pytest

        pytest.skip('whole module', allow_module_level=True)

        def test_never():
            assert 0
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 a failing unittest subTest
      Given a file "test_u.py" containing:
        """
        import unittest

        class T(unittest.TestCase):
            def test_sub(self):
                for i in range(3):
                    with self.subTest(i=i):
                        self.assertNotEqual(i, 1)

        def test_ok():
            pass
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

    Scenario: MG-01 a failing native subtests fixture
      Given a file "test_n.py" containing:
        """
        def test_native(subtests):
            for i in range(3):
                with subtests.test(i=i):
                    assert i != 2

        def test_ok():
            pass
        """
      When I run plain pytest with JUnit output
      And I run "rstest -n 0" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes
      When I run "rstest -n 2" with JUnit output
      Then rstest matches plain pytest: exit code, summary counts and per-test outcomes

  Rule: --basetemp in addopts gets xdist's per-worker layout

    Scenario: MG-02 a shared --basetemp root does not delete another worker's tmp_path
      # xdist gives each worker bt/gwN; a shared root lets one worker's startup
      # rm_rf delete another's live tmp_path. The race is timing-dependent, so
      # the parallel run repeats.
      Given a file "pytest.ini" containing:
        """
        [pytest]
        addopts = --basetemp=bt
        """
      And a file "test_tmp.py" containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(40))
        def test_tmp(tmp_path, i):
            (tmp_path / 'f.txt').write_text(str(i))
            time.sleep(0.01)
            assert (tmp_path / 'f.txt').read_text() == str(i)
        """
      When I run "rstest -n 0"
      Then the run succeeds
      And stdout contains "40 passed"
      When I run "rstest -n 4" 5 times, noting the directories after each
      Then every run succeeded with "40 passed" in the last line of stdout
      And after every run "bt/gw0", "bt/gw1", "bt/gw2" and "bt/gw3" were directories

  Rule: a failing unittest subTest is a failure in every artifact

    Background:
      Given a file "test_u.py" containing:
        """
        import unittest

        class T(unittest.TestCase):
            def test_sub(self):
                for i in range(3):
                    with self.subTest(i=i):
                        self.assertNotEqual(i, 1)

        def test_ok():
            pass
        """

    Scenario: MG-03 at -n 2 the exit code, summary, JSON report and JUnit all count it
      When I run plain pytest
      Then plain pytest exited with code 1
      And the last line of plain pytest's stdout contains "1 failed"
      When I run "rstest -n 2" with a JSON report and JUnit output
      Then the exit code is 1
      And the last line of stdout matches "1 failed"
      And the summary counts match plain pytest's
      And the JSON report's meta.counts.failed is at least 1
      And the JUnit report has a <failure> for "test_u.T::test_sub"

    Scenario: MG-03 at -n 0 the JSON report counts it like the terminal summary
      # The terminal summary here is pytest's own; the recorder must not keep
      # the parent test's call=passed.
      When I run "rstest -n 0" with a JSON report
      Then the exit code is 1
      And the last line of stdout matches "1 failed"
      And the JSON report's meta.counts.failed is at least 1

  Rule: the readiness tools catch what would break the parallel run

    Scenario: MG-04 migrate-check flags a hash-order-dependent collection
      # Parametrizing over a set gives a hash-randomized order per process.
      # xdist rejects it ("Different tests were collected"), and so does the
      # pool, so migrate-check must not call it ready.
      Given a file "test_set.py" containing:
        """
        import pytest

        NAMES = {'alpha', 'beta', 'gamma', 'delta', 'epsilon', 'zeta', 'eta', 'theta'}

        @pytest.mark.parametrize('n', NAMES)
        def test_n(n):
            pass
        """
      When I collect with plain pytest under PYTHONHASHSEED 1, 2 and 3
      Then the collected order is not the same every time, and each lists 8 tests
      When I run "rstest -n 4" with PYTHONHASHSEED unset
      Then the run fails
      And the output contains "different"
      When I run "rstest migrate-check" 3 times with PYTHONHASHSEED unset
      Then every run failed with "UNSTABLE NODEIDS: none" not in stdout

    Scenario: MG-05 migrate-check reports a parallel-only failure
      # Four tests contend on one fixed path (a create-exclusive lock): any
      # real parallel pass fails them.
      Given a file "test_lock.py" containing:
        """
        import os, time, pytest

        LOCK = os.path.join(os.path.dirname(__file__), 'shared.lock')

        @pytest.mark.parametrize('i', range(4))
        def test_lock(i):
            fd = os.open(LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            try:
                time.sleep(0.3)
            finally:
                os.close(fd)
                os.unlink(LOCK)
        """
      When I run "rstest -n 0"
      Then the run succeeds
      When I run "rstest -n 4"
      Then the exit code is 1
      And the last line of stdout matches "failed"
      When I run "rstest migrate-check"
      Then the run fails
      And stdout does not contain "PARALLEL: ready"

    # xdist-removal-check lists one finding per site, each with its own fix.
    # (`--xdist-trial` with xdist installed is not covered: the worker venv has
    # no pytest-xdist and must not get one.)

    Scenario: MG-11 xdist-removal-check gives each xdist import its own fix
      Given a file "conftest.py" containing:
        """
        from xdist import is_xdist_worker
        import xdist.plugin as xp

        def pytest_configure(config):
            pass
        """
      And a file "test_a.py" containing:
        """
        def test_a():
            pass
        """
      When I run "rstest xdist-removal-check" with the findings JSON
      Then the run fails
      And there are exactly 2 import findings, on distinct locations
      And the fix for the import finding at "conftest.py:1" contains "is_xdist_worker"
      And the import finding at "conftest.py:2" has a fix that does not contain "is_xdist_worker"

    Scenario Outline: MG-11 xdist-removal-check lists the addopts '<addopts>' once with a fix
      Given a file "pytest.ini" containing:
        """
        [pytest]
        addopts = <addopts>
        """
      And a file "test_a.py" containing:
        """
        def test_a():
            pass
        """
      When I run "rstest xdist-removal-check" with the findings JSON
      Then the run fails
      And exactly one finding is located in addopts, its text contains "<needle>" and it has a fix

      Examples:
        | addopts          | needle          |
        | -nauto           | -n              |
        | -n=4             | -n              |
        | --numprocesses 4 | --numprocesses  |
        | --dist loadscope | --dist          |
        | -p xdist         | -p xdist        |

  Rule: options shared with plugins keep working

    Scenario: MG-06 a conftest --output option reaches the plugin or rstest refuses loudly
      # Either the value reaches the plugin, or rstest exits 4 and points at
      # the `--` escape.
      Given a file "conftest.py" containing:
        """
        def pytest_addoption(parser):
            parser.addoption('--output', default='test-results')
        """
      And a file "test_o.py" containing:
        """
        def test_o(request):
            assert request.config.getoption('--output') == 'artifacts'
        """
      When I run plain pytest with "--output artifacts"
      Then plain pytest exited with code 0
      When I run "rstest -n 0 --output artifacts"
      Then the run succeeds with "1 passed" in stdout, or exits 4 with "-- --output artifacts" in the output
      When I run "rstest -n 2 --output artifacts"
      Then the run succeeds with "1 passed" in stdout, or exits 4 with "-- --output artifacts" in the output
      When I run "rstest -n 2 -- --output artifacts"
      Then the run succeeds
      And stdout contains "1 passed"

    Scenario: MG-07 the docs list every rstest option a popular plugin also defines
      # Hand-maintained list; only flags that --help actually lists are linted.
      When I run "rstest --help"
      Then the help lists the options "--output", "--timeout", "--reruns" and "--html"
      And each of these options that the help lists is in the first column of docs/_snippets/shadowed-flags.md:
        | flag           | also defined by      |
        | --output       | pytest-playwright    |
        | --timeout      | pytest-timeout       |
        | --reruns       | pytest-rerunfailures |
        | --only-rerun   | pytest-rerunfailures |
        | --html         | pytest-html          |
        | --junitxml     | pytest core          |
        | --debug        | pytest core          |
        | --dist         | pytest-xdist         |
        | --numprocesses | pytest-xdist         |

  Rule: worker identity matches xdist

    Scenario: MG-09 testrun_uid is xdist-shaped and identical on every worker
      Given the fixture event logger in "conftest.py"
      And 4 files "test_u{i}.py" each containing:
        """
        import os, uuid
        from conftest import _log

        def test_shape(testrun_uid):
            _log(uid=testrun_uid, env=os.environ.get('PYTEST_XDIST_TESTRUNUID'))

        def test_uuid(testrun_uid):
            uuid.UUID(testrun_uid)
            assert len(os.environ['PYTEST_XDIST_TESTRUNUID']) == 32
        """
      When I run "rstest -n 2" collecting the fixture event log
      Then 4 events came from 2 distinct workers, all with one uid equal to its env
      # test_uuid asserts the 32-hex uuid shape on every worker.
      And the run succeeds
      And stdout contains "8 passed"

  Rule: scheduling marks behave like xdist's

    Scenario: MG-08 loadgroup puts each xdist_group on its own worker
      # Two xdist_groups spread over four files, plus free tests. The groups
      # must run side by side, not one after the other.
      Given the fixture event logger in "conftest.py"
      And 4 files "test_g{i}.py" each containing:
        """
        import time, pytest
        from conftest import _log

        def _run(name):
            start = time.time()
            time.sleep(0.1)
            _log(name=name, start=start, end=time.time())

        @pytest.mark.xdist_group('db')
        def test_db():
            _run('db')

        @pytest.mark.xdist_group('net')
        def test_net():
            _run('net')

        def test_free():
            _run('free')
        """
      When I run "rstest -n 4 --dist loadgroup -v" 3 times collecting the fixture event log
      Then every run succeeded with 12 events, the "db" and the "net" events each on one worker
      And in every run the "db" and "net" groups ran on different workers
      And in every run the "db" and "net" groups overlapped in time

    Scenario: MG-12 serial tests reuse the designated worker's session
      # Ordering and exclusivity of serial tests are covered elsewhere; this
      # pins that no second session fixture setup happens for them.
      Given the fixture event logger in "conftest.py", followed by:
        """
        import pytest

        @pytest.fixture(scope='session', autouse=True)
        def sess():
            _log(ev='sess-up')
            yield
            _log(ev='sess-down')
        """
      And a file "test_s.py" containing:
        """
        import time, pytest
        from conftest import _log

        @pytest.mark.parametrize('i', range(6))
        def test_par(i):
            time.sleep(0.05)
            _log(ev='par')

        @pytest.mark.serial
        def test_serial_one():
            _log(ev='serial')

        @pytest.mark.serial
        def test_serial_two():
            _log(ev='serial')
        """
      When I run "rstest -n 3" collecting the fixture event log
      Then the run succeeds
      And stdout contains "8 passed"
      And both "serial" events came from one worker
      And no "sess-up" event falls between the serial events, and the serial worker has exactly one

    Scenario: MG-13 the docs agree on what a reordering modifyitems hook does
      When I read what docs/guides/migrate-from-pytest.md and docs/reference/xdist-support.md say about modifyitems reordering
      Then both docs make the same claim about whether it is ignored

    Scenario: MG-13 a cold-cache -n 2 run dispatches in the hook's reversed order
      Given the fixture event logger in "conftest.py", followed by:
        """
        def pytest_collection_modifyitems(items):
            items.reverse()
        """
      And a file "test_r.py" containing:
        """
        import time, pytest
        from conftest import _log

        @pytest.mark.parametrize('i', range(20))
        def test_r(i):
            time.sleep(0.02)
            _log(i=i)
        """
      When I run "rstest -n 2" collecting the fixture event log
      Then the run succeeds
      And 20 events were logged, each worker's "i" values descend, and 19 is among the first two

  Rule: --collect lazy finds the same tests as pytest

    Scenario: MG-14 the concepts pages that describe collection point at lazy collection
      # Large warm-cache runs collect lazily by default; a page that explains
      # collection as "every worker collects the whole suite" must say so.
      Given the level-2 section "## How a parallel run works" of "docs/concepts/architecture.md"
      Then that level-2 section contains "lazy-collection.md" or "--collect lazy"
      Given the level-2 section "## Collection and verification" of "docs/concepts/scheduling.md"
      Then that level-2 section contains "lazy-collection.md" or "--collect lazy"

    Scenario: MG-10 overriding norecursedirs collects a hidden directory
      # Overriding norecursedirs drops pytest's default '.*'.
      Given a file "pytest.ini" containing:
        """
        [pytest]
        norecursedirs = legacy
        """
      And a file "tests/test_a.py" containing:
        """
        def test_a():
            pass
        """
      And a file "tests/.hidden/test_h.py" containing:
        """
        def test_h():
            pass
        """
      And a file "legacy/test_old.py" containing:
        """
        def test_old():
            assert 0
        """
      When I run plain pytest
      Then plain pytest exited with code 0
      And plain pytest's summary counts 2 passed
      When I run "rstest -n 2 --collect lazy"
      Then rstest matches plain pytest: exit code and summary counts

    Scenario: MG-10 a ripgrep-style .ignore file means nothing to collection
      Given a file ".ignore" containing:
        """
        b/
        """
      And a file "a/test_a.py" containing:
        """
        def test_a():
            pass
        """
      And a file "b/test_b.py" containing:
        """
        def test_b():
            pass
        """
      When I run plain pytest
      Then plain pytest exited with code 0
      And plain pytest's summary counts 2 passed
      When I run "rstest -n 2 --collect lazy"
      Then rstest matches plain pytest: exit code and summary counts

    Scenario: MG-10 glob testpaths
      Given a file "pytest.ini" containing:
        """
        [pytest]
        testpaths = pkgs/*/tests
        """
      And a file "pkgs/one/tests/test_1.py" containing:
        """
        def test_1():
            pass
        """
      And a file "pkgs/two/tests/test_2.py" containing:
        """
        def test_2():
            pass
        """
      And a file "other/test_x.py" containing:
        """
        def test_x():
            assert 0
        """
      When I run plain pytest
      Then plain pytest exited with code 0
      And plain pytest's summary counts 2 passed
      When I run "rstest -n 2 --collect lazy"
      Then rstest matches plain pytest: exit code and summary counts
