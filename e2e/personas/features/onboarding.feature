Feature: First-time evaluator onboarding
  A developer installs rstest into an existing pytest project, runs
  `rstest try` and plain `rstest`, makes the usual newbie mistakes, and
  reads the output.

  Rule: rstest try gives an honest verdict

    Scenario: EV-01 a healthy suite reports parity and the worker count used
      Given 3 files "test_ok{i}.py" each containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(6))
        def test_t(i):
            time.sleep(0.05)
        """
      When I run "rstest try"
      Then the run succeeds
      And stdout contains "18 tests"
      And stdout contains "identical outcomes"
      And the stdout line containing "speed:" matches "-n \d+"

    Scenario: EV-02 a collection error is not blessed as drop-in ready
      Given a file "test_ok.py" containing:
        """
        def test_ok(): pass
        """
      And a file "test_bad.py" containing:
        """
        def test_x(:
            pass
        """
      When I run "rstest try"
      Then stdout does not contain "drop-in ready"
      And the run fails

    Scenario: EV-03 an empty project is not parity
      Given an empty file ".keep"
      When I run "rstest try"
      Then stdout does not contain "drop-in ready"
      And the run fails

    Scenario: EV-04 failing unittest subtests are not hidden behind a parity verdict
      # Several files so -n auto really builds a pool (at one worker the
      # outcome is pytest's own and correct).
      Given 4 files "test_sub{i}.py" each containing:
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
      When I run "rstest try"
      Then stdout does not contain "(0 failing)"
      And try does not silently report the suite as drop-in ready

  Rule: a zero-config first run is parallel and pytest-shaped

    Scenario: EV-05 plain rstest on a multi-file suite runs in parallel
      Given 4 files "test_f{i}.py" each containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(10))
        def test_f(i):
            time.sleep(0.05)
        """
      When I run "rstest" and note the worker count
      Then the run succeeds
      And the worker count is at least 2
      And stdout contains "40 passed"

    Scenario: EV-06 a warm one-file wait-bound suite is not capped at one worker
      # --dist load splits within a file, so once the duration cache is warm
      # -n auto should use more than one worker.
      Given a file "test_io.py" containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(20))
        def test_io(i):
            time.sleep(0.2)
        """
      And I have run "rstest -q"
      When I run "rstest -q" and note the worker count
      Then the run succeeds
      And the worker count is at least 2

    Scenario: EV-07 selecting one test out of many files runs single-worker
      Given 12 files "tests/test_{i}.py" each containing:
        """
        def test_{i}():
            pass
        """
      When I run "rstest tests/test_1.py::test_1" and note the worker count
      Then the run succeeds
      And the worker count is 1

  Rule: interpreter discovery explains itself

    Scenario: EV-08 a project .venv without rstest is named, not silently skipped
      # The project .venv has the project's deps but rstest was never installed
      # into it; another interpreter with rstest is on PATH.
      Given a project .venv that has the project's deps but not rstest
      And the worker interpreter is first on PATH
      When I run "rstest -n 2" with VIRTUAL_ENV unset
      Then the run fails
      And the output mentions the missing dependency or that rstest is not installed in the .venv
      And the output contains ".venv"

    Scenario: EV-09 a stale .python-version pin is soft, or its file is named
      Given a usable project .venv
      And a .python-version pinning a different minor than the .venv
      When I run "rstest -n 2" with VIRTUAL_ENV unset
      Then the run succeeds, or the project .venv was rejected only for the version pin
      And the run succeeds, or the output contains ".python-version"

  Rule: newbie mistakes get one clear error

    Background:
      Given 4 files "tests/test_{i}.py" each containing:
        """
        def test_a():
            pass
        """

    Scenario: EV-10 a nonexistent nodeid exits 4 with one error
      When I run "rstest -n 4 tests/test_1.py::nope"
      Then the exit code is 4
      And the output contains "not found:" exactly once

    Scenario: EV-10 a missing path exits 4 with one error
      When I run "rstest -n 4 tests/missing.py"
      Then the exit code is 4
      And the output contains "file or directory not found" exactly once

    Scenario: EV-11 a typo'd flag exits 4 with one error and no pytest.main() usage
      # pytest has --lf / --last-failed, not --lastfailed.
      When I run "rstest -n 4 --lastfailed"
      Then the exit code is 4
      And the output contains "unrecognized arguments" exactly once
      And the output does not contain "pytest.main()"

    Scenario: EV-12 a conftest that cannot import prints one traceback, not one per worker
      Given a file "tests/conftest.py" containing:
        """
        import onb_nonexistent_mod
        """
      When I run "rstest -n 3"
      Then the run fails
      And the output contains "ImportError while loading conftest" exactly once

    Scenario: EV-14 a global option before the subcommand is not a test path
      When I run "rstest -q try"
      Then the output does not contain "file or directory not found: try"

  Rule: the run location does not matter

    Scenario: EV-13 a subdirectory run uses the rootdir cache
      Given a file "pyproject.toml" containing:
        """
        [tool.pytest.ini_options]
        testpaths = ['tests']
        """
      And a file "tests/unit/test_a.py" containing:
        """
        def test_a():
            pass
        """
      And a file "tests/unit/test_b.py" containing:
        """
        def test_b():
            pass
        """
      When I run "rstest -n 2"
      Then the run succeeds
      And ".rstest_cache" is a directory
      When I run "rstest -n 2" in "tests/unit"
      Then the run succeeds
      And "tests/unit/.rstest_cache" does not exist

  Rule: the summary reads like pytest's

    Scenario Outline: EV-15 singular nouns in the summary with -n <workers>
      Given a file "test_a.py" containing:
        """
        import warnings, pytest

        @pytest.fixture
        def broken():
            raise RuntimeError('setup boom')

        def test_err(broken):
            pass

        def test_ok():
            pass
        """
      And a file "test_b.py" containing:
        """
        import warnings

        def test_warn():
            warnings.warn('old', DeprecationWarning)
        """
      When I run "rstest -n <workers>"
      Then the last line of stdout matches "\b1 error\b(?!s)"
      And the last line of stdout matches "\b1 warning\b(?!s)"

      Examples:
        | workers |
        | 0       |
        | 2       |

  Rule: the docs a newcomer reads agree with rstest and with each other

    Scenario: EV-16 the "Start from scratch" walkthrough runs as its page says
      # The page's own code blocks, run in order. Step 5's prose must count
      # what steps 2 to 4 built (three tests in one file, run on one worker),
      # then what test_slow.py brings (twelve tests in one selected file, which
      # a cold -n auto runs on one worker). Sleeps are shortened for speed.
      Given the python blocks of "docs/getting-started/your-first-test.md" containing "def test_add", joined, as "test_first.py"
      When I run "rstest" and note the worker count
      Then the exit code is 1
      And stdout contains "1 failed, 2 passed"
      And the worker count is 1
      Given the prose before code block 1 of the "## 5. Watch it go parallel" section of "docs/getting-started/your-first-test.md"
      Then every count that prose states matches 3 tests, 1 file and 1 worker
      Given the python blocks of "docs/getting-started/your-first-test.md" containing "# test_slow.py", joined, as "test_slow.py"
      And in "test_slow.py", "time.sleep(1)" is replaced by "time.sleep(0.02)"
      When I run "rstest test_slow.py" and note the worker count
      Then the run succeeds
      And stdout contains "12 passed"
      And the worker count is 1
      Given the prose before code block 2 of the "## 5. Watch it go parallel" section of "docs/getting-started/your-first-test.md"
      Then every count that prose states matches 12 tests, 1 file and 1 worker
      When I run "rstest -n 4 test_slow.py" and note the worker count
      Then the run succeeds
      And the worker count is 4

    Scenario: EV-17 the Guides nav mirrors the guides index page
      Then the mkdocs nav's "Guides" section lists the groups and pages of "docs/guides/index.md", in order
      And the mkdocs nav has no top-level "Playbooks" section

    Scenario: EV-18 every glossary term is linkable and easy to find
      Then every term in "docs/concepts/glossary.md" has an explicit anchor id
      And the terms in each section of "docs/concepts/glossary.md" are in alphabetical order

    Scenario: EV-19 the hang-watchdog formula has one home, and restatements link to it
      Then every docs paragraph outside "concepts/crash-handling.md" that matches "timeout \+ 10" links to "crash-handling.md#"
