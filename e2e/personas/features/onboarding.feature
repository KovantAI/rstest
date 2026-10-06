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
      Given the python blocks of "docs/getting-started/start-from-scratch.md" containing "def test_add", joined, as "test_first.py"
      When I run "rstest" and note the worker count
      Then the exit code is 1
      And stdout contains "1 failed, 2 passed"
      And the worker count is 1
      Given the prose before code block 1 of the "## 5. Watch it go parallel" section of "docs/getting-started/start-from-scratch.md"
      Then every count that prose states matches 3 tests, 1 file and 1 worker
      Given the python blocks of "docs/getting-started/start-from-scratch.md" containing "# test_slow.py", joined, as "test_slow.py"
      And in "test_slow.py", "time.sleep(1)" is replaced by "time.sleep(0.02)"
      When I run "rstest test_slow.py" and note the worker count
      Then the run succeeds
      And stdout contains "12 passed"
      And the worker count is 1
      Given the prose before code block 2 of the "## 5. Watch it go parallel" section of "docs/getting-started/start-from-scratch.md"
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

    Scenario: EV-20 only session fixtures are said to run once per worker
      # A module-scoped fixture runs on every worker that gets tests from its
      # module, which is not "once per worker".
      Then no paragraph of "README.md" matches "(?i)module-scoped fixtures? (run|instantiate) \[?once per worker"
      And no paragraph of "docs/**/*.md" matches "(?i)module-scoped fixtures? (run|instantiate) \[?once per worker"

    Scenario: EV-21 the features page does not claim to list every flag
      Then no paragraph of "docs/getting-started/features.md" matches "(?i)full surface|every flag|all flags"

    Scenario: EV-22 the first-run page names every selector rstest adds
      Given the level-2 section "## Selecting tests" of "docs/getting-started/run-your-suite.md"
      Then that level-2 section names each of "--changed --changed-strict --since-green --incremental"
      And no paragraph of "docs/getting-started/run-your-suite.md" matches "(?i)one selector"

    Scenario: EV-23 --dist each is not sold as heterogeneous-environment testing
      # Every worker uses the same interpreter; xdist's --tx has no equivalent.
      Then no paragraph of "docs/**/*.md" matches "(?i)configured differently"

    Scenario: EV-24 path arguments are not offered as a way to split a monorepo
      # A path argument opts out of monorepo mode: one session from the root,
      # under the root's config and interpreter.
      Then no paragraph of "docs/**/*.md" matches "(?i)(split|shard)[^.]*path arguments"
      And no fenced block in "docs/guides/ci-quickstart.md" contains "its .rstest_cache"

    Scenario: EV-25 sample diff-coverage reports add up
      Then every diff-coverage report in the docs counts as many uncovered lines as it lists

    Scenario: EV-26 the SQLAlchemy parity figures match the corpus results
      Then every docs paragraph on "sqlalchemy" parity states the score and mismatch count in "corpus/results.json"

    Scenario: EV-27 the pytest-django SQLite caveat has one home, and restatements link to it
      Then every docs paragraph outside "reference/corpus-plugins.md" that matches "(?s)pytest-django.*SQLite|SQLite.*pytest-django" links to "corpus-plugins.md#what-pytest-djangos-evidence-covers"

    Scenario: EV-28 the rerun-plugin mechanism has one home, and mentions link to it
      # pytest-retry's server_port and pytest-rerunfailures' sock_port were
      # explained in full on several pages; xdist-hooks.md now owns the how.
      Then every docs paragraph outside "concepts/xdist-hooks.md" that matches "server_port|sock_port|ReportServer" links to "xdist-hooks.md#"

    Scenario: EV-29 one name per concept
      # Maturity is "alpha (0.x)", as PyPI's classifier says; scheduling by
      # cached duration is "duration-aware scheduling", as the glossary says.
      Then no paragraph of "README.md" matches "(?i)pre-1\.0"
      And no paragraph of "docs/**/*.md" matches "(?i)pre-1\.0|long-pole-first|`RSTEST_RUN_UID` \| the run id\b"

    Scenario: EV-30 the project-status blurb defers to the evaluator's Maturity list
      Given the level-2 section "## Project status" of "docs/getting-started/getting-help.md"
      Then that level-2 section names the link "evaluating.md#maturity"
      And no paragraph of "docs/getting-started/getting-help.md" matches "(?i)two weeks|long-term-support"

    Scenario: EV-31 the README pitch is the docs home page's Highlights
      # GitHub and PyPI can't include docs snippets, so the README carries a
      # copy; it drifted from the docs before (and claimed module fixtures run
      # once per worker).
      Then the README list under the "docs/index.md" marker is the "## Highlights" list of "docs/index.md", with site links

    Scenario: EV-32 each plugin's verdict lives in one guide table, agreeing with the top-100 matrix
      Then no plugin has a verdict row on more than one page of "docs/guides/*.md"
      And every plugin in "docs/guides/plugins.md" has the verdict and V/i mark of "docs/reference/top-100-plugins.md"

    Scenario: EV-33 Windows caveats have one home, and restatements link to it
      Then every docs paragraph outside "guides/windows.md" that matches "no SIGALRM|no child CPU|Windows-heavy|any test on Windows|anonymous-pipe|On Windows[^.]{0,60}(--timeout|watchdog|fork|journal)" links to "windows.md#"

    Scenario: EV-34 the --since-green fingerprint is described as the code computes it
      # incremental.rs folds installed distributions into the fingerprint
      # (unit test env_fingerprint_reflects_dist_info_records), so an in-place
      # upgrade IS detected.
      Then no paragraph of "docs/**/*.md" matches "(?i)upgraded in place without a lockfile change is not detected|fingerprint \(interpreter and dependency manifests\)"

    Scenario: EV-35 a getting-started page's URL says what the page is
      Then the mkdocs nav lists "Run your existing suite" at "getting-started/run-your-suite.md"
      And the mkdocs nav lists "Start from scratch" at "getting-started/start-from-scratch.md"
      And mkdocs.yml redirects "getting-started/first-steps.md" to "getting-started/run-your-suite.md"
      And mkdocs.yml redirects "getting-started/your-first-test.md" to "getting-started/start-from-scratch.md"

    Scenario: EV-36 the existing-suite page doesn't borrow the walkthrough's file name
      # start-from-scratch.md builds test_first.py with three tests; a different
      # test_first.py here (test_add_zero, test_skipped) confused readers of both.
      Then no fenced block in "docs/getting-started/run-your-suite.md" contains "test_first.py"

    Scenario: EV-37 the sample try report shows what a git checkout prints
      # try_cmd.rs adds the 30-day projection whenever git history is there,
      # which is the usual case; the arithmetic must hold.
      Then every `rstest try` saves line in "docs/**/*.md" has the report's 30-day projection

    Scenario: EV-38 the refusal is tied to workers disagreeing, not to full collection itself
      Then no paragraph of "docs/**/*.md" matches "(?i)refuses to dispatch whenever every worker collects"

    Scenario: EV-39 the sample run's page quotes the measured allauth time
      Then the intro of "docs/getting-started/run-your-suite.md" quotes the django-allauth `-n 4` time from "docs/reference/benchmarks.md"

    Scenario: EV-40 migrate-check's parallel pass is never described as -n auto
      # migrate::parallel_n uses run::check_workers: never capped by the
      # duration cache, never below 2.
      Then no paragraph of "docs/reference/report-json.md" matches "`-n auto` classification"

    Scenario: EV-41 every statement of the parallel-floor threshold includes the 10% slack
      # doctor::FLOOR_SLACK = 1.1: a balanced pool sitting at the share is not flagged.
      Then no paragraph of "docs/**/*.md" matches "(?i)(both|than) (the ideal per-worker share|that share) and 1 second(?!, plus 10%)"

    Scenario: EV-42 the two line-number conventions are called out where each is defined
      # report-json lineno is pytest's 0-based location; explain's source_line
      # is 1-based. Off-by-one in editor integrations otherwise.
      Given the "### `explain`" section of "docs/reference/cli-commands.md"
      Then that docs section contains "`source_line` is **1-based**"
      Given the table row of "docs/reference/report-json.md" starting with "| `lineno` | int | **0-based**"
      Then that docs section contains "1-based `source_line`"

    Scenario: EV-43 every environment variable rstest reads is documented
      # PATH/PATHEXT are the OS's; the rest of the exceptions are developer-only
      # test and benchmark knobs.
      Then every environment variable the CLI reads is named in "docs/reference/environment.md", except "PATH PATHEXT RSTEST_BENCH_CYCLES RSTEST_BENCH_EDITS RSTEST_BENCH_FILES RSTEST_BLESS_SCHEMAS RSTEST_TEST_PYTHON"

    Scenario: EV-44 the quarantine example shows a run that quarantine turns green
      # A stray "1 failed" next to the exit-0 rule read as a contradiction.
      Then no fenced block in "docs/guides/flaky-tests.md" contains "failed, 41 passed, 1 quarantined"

    Scenario: EV-45 explain is not described as reading replay journals
      # explain reads durations, flakes and the coverage index, never journals.
      Then no paragraph of "docs/**/*.md" matches "(?i)replay journal for `rstest replay` or `rstest explain`"

    Scenario: EV-46 "byte-exact" single-worker output always carries its --output exception
      # An explicit --output switches -n 0 to rstest's renderer (compatibility.md).
      Then no paragraph of "docs/**/*.md" matches "^(?!.*--output).*byte-exact pytest output"

    Scenario: EV-47 the shadowed-flags table stays scannable
      # Edge cases go in the note under the table, not into a cell.
      Then no table cell in "docs/_snippets/shadowed-flags.md" is longer than 250 characters
