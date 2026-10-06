Feature: CI / platform engineer
  A platform engineer wires rstest into a pipeline: copies a recipe from the
  docs, adds JUnit / report-json / annotation output, shards across jobs, and
  gates the build on the exit code and the artifacts. The exit code and every
  artifact must agree, documented snippets must fail the job when a test
  fails, and tool-side problems must never flip a test result.

  Rule: the exit code follows the documented table (docs/reference/exit-codes.md)

    Background:
      # One suite per situation; the .git dir bounds rootdir discovery.
      Given the directory ".git"
      And a file "pass/test_p.py" containing:
        """
        def test_a():
            pass

        def test_b():
            pass
        """
      And a file "fail/test_f.py" containing:
        """
        def test_a():
            pass

        def test_b():
            assert 0
        """
      And a file "coll/test_ok.py" containing:
        """
        def test_a():
            pass
        """
      And a file "coll/test_c.py" containing:
        """
        import ci_nonexistent_mod
        """
      And a file "skip/test_s.py" containing:
        """
        import pytest

        @pytest.mark.skip
        def test_a():
            pass

        @pytest.mark.skip
        def test_b():
            pass
        """
      And a file "xpass/test_x.py" containing:
        """
        import pytest

        @pytest.mark.xfail(strict=True)
        def test_a():
            pass

        def test_b():
            pass
        """
      And an empty file "empty/.keep"

    Scenario Outline: CI-01 setup: pytest exits <exit> on <situation>, as the table documents
      When I run "-q -p no:cacheprovider <args>" under plain pytest from the worker venv
      Then the exit code is <exit>

      Examples:
        | situation          | args                 | exit |
        | all pass           | pass                 | 0    |
        | one failure        | fail                 | 1    |
        | collection error   | coll                 | 2    |
        | bad flag           | pass --ci-bogus-flag | 4    |
        | bad -m expression  | pass -m 'a and'      | 4    |
        | missing path       | pass/missing.py      | 4    |
        | no tests           | empty                | 5    |
        | -k matches nothing | pass -k ci_nomatch   | 5    |
        | all skipped        | skip                 | 0    |
        | strict xpass       | xpass                | 1    |

    Scenario Outline: CI-01 -n <mode> <situation>: exit <exit> and report-json meta.exitstatus agrees
      When I run "rstest -n <mode> <args> --report-json report.json"
      Then the exit code is <exit>
      And report-json "report.json" records meta.exitstatus <exit>

      Examples:
        | mode | situation          | args                 | exit |
        | 0    | all pass           | pass                 | 0    |
        | 0    | one failure        | fail                 | 1    |
        | 0    | collection error   | coll                 | 2    |
        | 0    | bad flag           | pass --ci-bogus-flag | 4    |
        | 0    | bad -m expression  | pass -m 'a and'      | 4    |
        | 0    | missing path       | pass/missing.py      | 4    |
        | 0    | no tests           | empty                | 5    |
        | 0    | -k matches nothing | pass -k ci_nomatch   | 5    |
        | 0    | all skipped        | skip                 | 0    |
        | 0    | strict xpass       | xpass                | 1    |
        | 2    | all pass           | pass                 | 0    |
        | 2    | one failure        | fail                 | 1    |
        | 2    | collection error   | coll                 | 2    |
        | 2    | bad flag           | pass --ci-bogus-flag | 4    |
        | 2    | bad -m expression  | pass -m 'a and'      | 4    |
        | 2    | missing path       | pass/missing.py      | 4    |
        | 2    | no tests           | empty                | 5    |
        | 2    | -k matches nothing | pass -k ci_nomatch   | 5    |
        | 2    | all skipped        | skip                 | 0    |
        | 2    | strict xpass       | xpass                | 1    |

    @posix_only
    Scenario Outline: CI-01 -n <mode> SIGINT: exit 2 and report-json meta.exitstatus agrees
      # A CI runner's cancel (or Ctrl-C) delivers SIGINT to the process group;
      # pytest exits 2 with a KeyboardInterrupt summary.
      Given a file "sigint/test_slow.py" containing:
        """
        import time

        def test_quick():
            pass

        def test_slow_a():
            time.sleep(30)

        def test_slow_b():
            time.sleep(30)
        """
      When I run "rstest -n <mode> sigint --report-json report.json" and SIGINT its process group after 2 seconds
      Then the exit code is 2
      And report-json "report.json" records meta.exitstatus 2

      Examples:
        | mode |
        | 0    |
        | 2    |

    Scenario Outline: CI-07 --shard <spec>: a matrix typo is rejected with an error naming --shard
      When I run "rstest -n 2 --shard=<spec> pass"
      Then the exit code is neither 0 nor 5
      And stderr contains "--shard"
      And stdout does not contain " passed"

      Examples:
        | spec |
        | 0/4  |
        | 5/4  |
        | 1/0  |
        | a/b  |
        | 2    |
        | -1/4 |

    Scenario: CI-07 --shard k/4 with 2 tests: surplus shards run nothing and still reconcile
      # Same (cold) cache on every job, or a sibling's timings reshuffle the
      # partition between shards.
      When I run "rstest -n 2 pass" as shards 1/4 to 4/4, each from a cold cache
      Then every shard exits 0 or 5
      And every shard's report-json is stamped with its own index in meta.shard.k
      When I run shard-verify over the shard reports
      Then the run succeeds

  Rule: artifacts agree with the exit code (CI-02)

    Scenario: CI-02 setup: the hand-rolled schema check rejects a malformed report
      Given a file "bogus.json" containing:
        """
        {"collect_errors": [1], "meta": {"counts": {"x": -1}}, "tests": {"t": {"call": 0}}}
        """
      Then the report-json schema check finds at least 4 errors in "bogus.json"

    # Each suite is two files so -n 2 really spreads work over a pool.

    Scenario: CI-02 green: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
        """
        def test_a():
            pass

        def test_b():
            pass
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

    Scenario: CI-02 failing: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
        """
        def test_ok():
            pass

        def test_bad():
            assert 1 == 2
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

    Scenario: CI-02 fixture_errors: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
        """
        import pytest

        @pytest.fixture
        def broken():
            raise RuntimeError('setup boom')

        @pytest.fixture
        def bad_teardown():
            yield
            raise RuntimeError('teardown boom')

        def test_setup(broken):
            pass

        def test_teardown(bad_teardown):
            pass

        def test_ok():
            pass
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

    Scenario: CI-02 skip_xfail: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
        """
        import sys, pytest

        @pytest.mark.skip(reason='no')
        def test_skip():
            pass

        @pytest.mark.skipif(sys.platform != 'nope', reason='cond')
        def test_skipif():
            pass

        @pytest.mark.xfail
        def test_xfail():
            assert 0

        @pytest.mark.xfail
        def test_xpass():
            pass

        def test_ok():
            pass
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

    Scenario: CI-02 strict_xpass: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
        """
        import pytest

        @pytest.mark.xfail(strict=True)
        def test_xpass():
            pass

        def test_ok():
            pass
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

    Scenario: CI-02 ids_weird: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
        """
        import pytest

        @pytest.mark.parametrize('v', ['a::b', 'x[1]', 'p/q', 'sp ace', 'ünï', ']]>'])
        def test_ids(v):
            assert v != 'p/q'
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

    Scenario: CI-02 collect_error: every artifact agrees with the exit code
      Given the directory ".git"
      And a file "tests/test_ok.py" containing:
        """
        def test_a():
            pass
        """
      And a file "tests/test_c.py" containing:
        """
        import ci_nonexistent_mod
        """
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file
      And report-json "r.json" counts the collect error once, as "j.xml" does

    Scenario: CI-02 unittest_subtest: every artifact agrees with the exit code
      Given the directory ".git"
      And 2 files "tests/test_{i}.py" each containing:
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
      When I run "rstest -n 2 --junitxml j.xml --report-json r.json --html r.html"
      Then the JUnit report "j.xml" and the report-json "r.json" are readable
      And the exit code is non-zero exactly when "j.xml" has a failure or error element
      And the exit code is non-zero exactly when "r.json" counts a failure, error or collect error
      And report-json "r.json" records the exit code as meta.exitstatus
      And the testsuite counts in "j.xml" equal its child elements
      And report-json "r.json" matches the documented report-json schema
      And "r.html" is a non-empty file

  Rule: every annotation style makes a collect error visible to the CI system (CI-04)

    Background:
      Given the directory ".git"
      And a file "tests/test_ok.py" containing:
        """
        def test_a():
            pass
        """
      And a file "tests/test_c.py" containing:
        """
        import ci_nonexistent_mod
        """

    Scenario: CI-04 --output tap: a collect error is not a bare 1..0
      When I run "rstest -n 2 --output tap"
      Then the exit code is 2
      And a stdout line starts with "not ok" or "Bail out!"
      And the output contains "ci_nonexistent_mod"

    Scenario Outline: CI-04 --output <style>: a collect error exits 2 with the traceback and an annotation
      When I run "rstest -n 2 --output <style>"
      Then the exit code is 2
      And the output contains "ci_nonexistent_mod"
      And a stdout line starts with "<prefix>"

      Examples:
        | style  | prefix                              |
        | github | ::error file=tests/test_c.py        |
        | azure  | ##vso[task.logissue type=error      |

    Scenario: CI-04 --output teamcity: a collect error exits 2 with the traceback and an annotation
      When I run "rstest -n 2 --output teamcity"
      Then the exit code is 2
      And the output contains "ci_nonexistent_mod"
      And a stdout line starts with "##teamcity[" and contains "testFailed" or "buildProblem"

  Rule: annotations point at repo paths and carry the exception (CI-05)

    Background:
      # Monorepo layout from ci-quickstart: the project dir is the repo root
      # and the job runs with working-directory: proj.
      Given a file "proj/tests/test_a.py" containing:
        """
        import pytest

        @pytest.fixture
        def broken():
            raise RuntimeError('setup boom')

        def test_err(broken):
            pass

        def test_fail():
            x = 1
            assert x == 2, 'x mismatch'

        def test_ok():
            pass
        """
      And the project is a git repository with everything committed

    Scenario: CI-05 github: one ::error per failing test, repo-relative, with the exception line
      When I run "rstest -n 2 --output github" in "proj" with CI environment "GITHUB_ACTIONS=true GITHUB_WORKSPACE={project}"
      Then the exit code is 1
      And stdout has exactly 2 lines starting with "::error "
      And a stdout line starting with "::error " contains "RuntimeError: setup boom"
      And a stdout line starting with "::error " contains "AssertionError: x mismatch"
      And every stdout line starting with "::error " contains "file=proj/tests/test_a.py"

    Scenario: CI-05 azure: one logissue per failing test, carrying the exception line
      When I run "rstest -n 2 --output azure" in "proj" with CI environment "TF_BUILD=True"
      Then stdout has exactly 2 lines starting with "##vso[task.logissue type=error"
      And a stdout line starting with "##vso[task.logissue type=error" contains "RuntimeError: setup boom"
      And a stdout line starting with "##vso[task.logissue type=error" contains "AssertionError: x mismatch"

  Rule: shards are complete, disjoint and balanced (CI-06)

    Background:
      Given the directory ".git"
      And 8 files "tests/test_f{i}.py" each holding 50 trivial passing tests
      And a file "tests/test_long.py" containing:
        """
        import time

        def test_long():
            time.sleep(0.5)
        """

    Scenario: CI-06 warm cache: every job restores one snapshot of a warm-up run
      When I run "rstest -n 2 -q"
      Then the run succeeds
      And ".rstest_cache/durations.json" is a file
      When I run "rstest -n 2 -q" as shards 1/4 to 4/4, each from a copy of the current cache
      And I run shard-verify over the shard reports
      Then the run succeeds
      And the shard reports hold 401 tests in total
      And at least 3 shard reports lack "test_long", each holding between half and twice the mean of 401 tests over 4 shards

    Scenario: CI-06 cold cache: shards with no durations are still complete and balanced
      When I run "rstest -n 2 -q" as shards 1/4 to 4/4, each from a cold cache
      And I run shard-verify over the shard reports
      Then the run succeeds
      And the shard reports hold 401 tests in total
      And at least 3 shard reports lack "test_long", each holding between half and twice the mean of 401 tests over 4 shards

  Rule: tool-side failures never flip a test result (CI-08)

    Background:
      Given the directory ".git"

    Scenario: CI-08 an unwritable GITHUB_STEP_SUMMARY does not turn green red
      # act and container jobs; the doctor's job-summary publish is cosmetic.
      Given a file "tests/test_ok.py" containing:
        """
        def test_a():
            pass

        def test_b():
            pass
        """
      When I run "rstest -n 2 --doctor-json doctor.json --report-json report.json" with CI environment "GITHUB_ACTIONS=true GITHUB_STEP_SUMMARY={tmp}/ci_nonexistent_dir/summary.md"
      Then stdout contains "2 passed"
      And report-json "report.json" records meta.exitstatus 0
      And the run succeeds
      And stderr contains the GITHUB_STEP_SUMMARY path

    Scenario: CI-08 a missing buildkite-agent warns and exits 0
      Given a file "tests/test_ok.py" containing:
        """
        def test_a():
            pass

        def test_b():
            pass
        """
      When I run "rstest -n 2 --doctor-json doctor.json" with BUILDKITE=true and no buildkite-agent on PATH
      Then the run succeeds
      And stderr contains "buildkite-agent"

    # A remote whose segments/ is a plain file fails both ways regardless of
    # permissions (CI often runs as root). Per ci-shared-cache.md a failed
    # --cache-push only warns; a failed --cache-pull aborts before any test.

    Scenario: CI-08 a failed --cache-push warns and a green run stays exit 0
      Given a file "tests/test_ok.py" containing:
        """
        def test_a():
            pass

        def test_b():
            pass
        """
      And a cache remote outside the project whose "segments" is a plain file
      When I run "rstest -n 2 --cache-remote {remote} --cache-push" against that remote
      Then the exit code is 0
      And stderr contains "cache: push failed"

    Scenario: CI-08 a failed --cache-push warns and a red run stays exit 1
      Given a file "tests/test_bad.py" containing:
        """
        def test_a():
            pass

        def test_b():
            assert 0
        """
      And a cache remote outside the project whose "segments" is a plain file
      When I run "rstest -n 2 --cache-remote {remote} --cache-push" against that remote
      Then the exit code is 1
      And stderr contains "cache: push failed"

    Scenario: CI-08 a failed --cache-pull exits 1 before any test runs and writes no report
      Given a file "tests/test_ok.py" containing:
        """
        def test_a():
            pass

        def test_b():
            pass
        """
      And a cache remote outside the project whose "segments" is a plain file
      When I run "rstest -n 2 --cache-remote {remote} --cache-pull --report-json report.json" against that remote
      Then the exit code is 1
      And stderr contains "Error: pulling shared cache"
      And stdout does not contain " passed"
      And "report.json" does not exist

  Rule: a CI run leaves the working tree clean (CI-11)

    Background:
      Given a file "tests/test_a.py" containing:
        """
        def test_a():
            pass
        """

    Scenario: CI-11 setup: pytest's own cache dir ignores itself
      Given a file "tests/test_b.py" containing:
        """
        def test_b():
            pass
        """
      And a file ".gitignore" containing:
        """
        __pycache__/
        """
      And the project is a git repository with everything committed
      When I run "-q" under plain pytest from the worker venv with CI environment "CI=true"
      Then the git working tree is clean, untracked files included
      And ".pytest_cache" is a directory

    Scenario: CI-11 CI=true rstest -n 2 leaves a clean tree
      Given a file "tests/test_b.py" containing:
        """
        def test_b():
            pass
        """
      And a file ".gitignore" containing:
        """
        __pycache__/
        """
      And the project is a git repository with everything committed
      When I run "rstest -n 2 --junitxml {tmp}/ci_clean.xml" with CI environment "CI=true"
      Then the run succeeds
      And the git working tree is clean, untracked files included

    Scenario: CI-11 -p no:cacheprovider writes no .rstest_cache
      When I run "rstest -n 2 -p no:cacheprovider" with CI environment "CI=true"
      Then the run succeeds
      And ".rstest_cache" does not exist

  @posix_only
  Rule: documented CI snippets fail the job when a test fails (CI-09)

    Background:
      # One failing test, plus fake external CLIs; `rstest` on PATH wraps the
      # binary under test.
      Given bash is on PATH
      And the directory ".git"
      And a file "tests/test_a.py" containing:
        """
        def test_ok():
            pass

        def test_bad():
            assert 0
        """
      And a file "tests/test_b.py" containing:
        """
        def test_ok2():
            pass
        """
      And fake "az aws gsutil buildkite-agent pip" CLIs that log their calls, and an rstest wrapper, first on PATH

    Scenario: CI-09 Azure materialize-a-dir: the step fails when a test fails
      # Azure runs `script:` as a file under `bash --noprofile --norc` with no
      # errexit; the agent expands $(Var) macros before bash sees the script.
      Given the "- script:" literal of the first block in "docs/guides/ci-recipes.md" containing "az storage blob download-batch"
      Then the snippet contains "rstest"
      When the agent expands "$(System.JobPositionInPhase)" and "$(System.TotalJobsInPhase)" in the snippet to "1"
      And I run the snippet with "bash --noprofile --norc" in the suite
      Then stdout contains "1 failed"
      And the fake CLI log contains "upload-batch"
      And the run fails

    Scenario: CI-09 shared-cache retry (reachable remote): rstest ran, the step fails
      # As a GitHub `run:` step (`bash -e {0}`).
      Given the first block in "docs/guides/ci-shared-cache.md" containing "Error: pulling shared cache" and "tee"
      Then the snippet contains "rstest"
      Given REMOTE is an existing directory outside the project
      When I run the snippet with "bash --noprofile --norc -e" in the suite
      Then stdout contains "1 failed"
      And the run fails

    Scenario: CI-09 shared-cache retry (failed pull): rstest ran, the step fails
      Given the first block in "docs/guides/ci-shared-cache.md" containing "Error: pulling shared cache" and "tee"
      Then the snippet contains "rstest"
      Given REMOTE is a plain file outside the project
      When I run the snippet with "bash --noprofile --norc -e" in the suite
      Then stdout contains "1 failed"
      And the output contains "Error: pulling shared cache"
      And the run fails

    Scenario: CI-09 GitLab sharding: the job fails when a test fails and JUnit is written
      # Each `script:` line runs under errexit.
      Given the "script" list of the first block in "docs/guides/ci-recipes.md" containing "CI_NODE_INDEX" and "parallel:"
      Then the snippet contains "rstest"
      When I prefix the snippet with "set -eo pipefail"
      And I run the snippet with "bash --noprofile --norc" in the suite with CI environment "CI_NODE_INDEX=1 CI_NODE_TOTAL=1 GITLAB_CI=true"
      Then the run fails
      And stdout contains "1 failed"
      And "junit.xml" is a file

  @posix_only
  Rule: the bundled action's "Run rstest" step passes inputs through faithfully (CI-10)

    Background:
      # Run as GitHub runs composite bash steps, every IN_* input empty unless
      # set, against a fake rstest.
      Given bash is on PATH
      And the run script of the "Run rstest" step in .github/actions/rstest/action.yml
      And a fake rstest first on PATH that prints each argument as ARG<...> and exits with $FAKE_RC

    Scenario: CI-10 setup: the shard pair and quoted args reach rstest
      Then the snippet contains "IN_SHARD"
      Given the action step environment:
        """
        IN_SHARD=2
        IN_SHARD_TOTAL=4
        IN_ARGS=-k "a and b"
        """
      When I run the action step
      Then the run succeeds
      And rstest received "--shard" followed by "2/4"
      And rstest received the argument "a and b"

    Scenario: CI-10 the rstest exit code propagates to the step
      Given the action step environment:
        """
        FAKE_RC=1
        """
      When I run the action step
      Then the exit code is 1
      And GITHUB_OUTPUT contains "exit-code=1"

    Scenario Outline: CI-10 shard='<shard>' shard-total='<total>': ::error:: and the step fails
      Given the action step environment:
        """
        IN_SHARD=<shard>
        IN_SHARD_TOTAL=<total>
        """
      When I run the action step
      Then the run fails
      And the output contains "::error::"

      Examples:
        | shard | total |
        | 2     |       |
        |       | 4     |

    Scenario: CI-10 doctor-fail-on: the comma list is trimmed into one flag per condition
      Given the action step environment:
        """
        IN_DOCTOR_FAIL_ON=wait_pct>50 , wall_seconds > 100
        """
      And the action input IN_DOCTOR_FAIL_ON is padded with a space on each side
      When I run the action step
      Then the run succeeds
      And rstest received "--doctor-fail-on" values "wait_pct>50", "wall_seconds > 100"

    Scenario Outline: CI-10 rerun-on <raw>: reaches --only-rerun unmangled
      Given the action step environment:
        """
        IN_RERUN_ON=<raw>
        """
      And the action input IN_RERUN_ON is padded with a space on each side
      When I run the action step
      Then the run succeeds
      And rstest received "--only-rerun" followed by the unpadded IN_RERUN_ON input

      Examples:
        | raw                |
        | Connection\s+reset |
        | can't connect      |
        | say "hi"           |

  Rule: CI logs are non-interactive and honour the color switches (CI-12)

    Background:
      Given the directory ".git"
      And 4 files "test_{i}.py" each containing:
        """
        import time

        def test_a():
            time.sleep(0.4)

        def test_b():
            time.sleep(0.4)

        def test_c():
            assert 1 == 2
        """

    # Buildkite (and docker -t) give the job a pty with CI=true.

    @posix_only
    Scenario: CI-12 setup: an interactive pty run draws the live footer
      When I run "rstest -n 2" on a pseudo-terminal
      Then the exit code is 1
      And the output has cursor-movement sequences

    @posix_only
    Scenario: CI-12 CI=true on a pty: no cursor-movement sequences
      When I run "rstest -n 2" on a pseudo-terminal with CI environment "CI=true"
      Then the exit code is 1
      And the output has no cursor-movement sequences

    @posix_only
    Scenario: CI-12 NO_COLOR=1 on a pty: no SGR color codes
      When I run "rstest -n 2" on a pseudo-terminal with CI environment "NO_COLOR=1"
      Then the exit code is 1
      And the output has no SGR color codes

    @posix_only
    Scenario: CI-12 --color=no on a pty: no SGR color codes
      When I run "rstest -n 2 --color=no" on a pseudo-terminal
      Then the exit code is 1
      And the output has no SGR color codes

    # Piped (GitHub Actions, GitLab, Jenkins).

    Scenario: CI-12 CI=true piped: no cursor movement and no color
      When I run "rstest -n 2" with CI environment "CI=true"
      Then the exit code is 1
      And the output has no cursor-movement sequences
      And the output has no SGR color codes

    Scenario: CI-12 --color=yes piped: colored, still no cursor movement
      When I run "rstest -n 2 --color=yes" with CI environment "CI=true"
      Then stdout has SGR color codes
      And the output has no cursor-movement sequences

    Scenario: CI-12 setup: pytest colors piped output under FORCE_COLOR=1
      When I run "-p no:cacheprovider" under plain pytest from the worker venv with CI environment "FORCE_COLOR=1"
      Then stdout has SGR color codes

    Scenario: CI-12 FORCE_COLOR=1 piped: output is colored, as with pytest
      When I run "rstest -n 2" with CI environment "FORCE_COLOR=1"
      Then stdout has SGR color codes

  Rule: the pre-commit hook env does not hide the project interpreter (CI-13)

    Scenario: CI-13 a hook VIRTUAL_ENV wins discovery, is named, and --python overrides it
      # pre-commit runs `language: python` hooks with VIRTUAL_ENV set to the
      # hook's own env (rstest + pytest, none of the project's deps).
      Given the directory ".git"
      And a file "deps/ci_dep.py" containing:
        """
        VALUE = 1
        """
      And a file "tests/test_dep.py" containing:
        """
        import ci_dep

        def test_dep():
            assert ci_dep.VALUE == 1
        """
      And a file "tests/test_more.py" containing:
        """
        def test_more():
            pass
        """
      And a project .venv that sees the worker venv's packages and "deps"
      And a pre-commit hook env that sees only the worker venv's packages
      When I run "rstest -n 2" with VIRTUAL_ENV unset
      Then the run succeeds
      And stdout contains "2 passed"
      When I run "rstest -n 2" with VIRTUAL_ENV set to the hook env
      Then the run fails
      And the output contains "ci_dep"
      And the output names the hook env or contains ".venv"
      When I run "rstest -n 2" with VIRTUAL_ENV set to the hook env and --python pointing at the project .venv
      Then the run succeeds
      And stdout contains "2 passed"

    Scenario: CI-13 docs: the pre-commit section shows a hook that uses the project interpreter
      Given the level-2 section "## Pre-commit" of "docs/guides/ci-recipes.md"
      Then that level-2 section contains "language: system" or "--python"

  Rule: documented CI setups are safe to copy (CI-14)

    Scenario: CI-14 the doctor-baseline workflow saves its cache only from main
      # A PR job that can save would read its own earlier baseline back on the
      # next push, so "vs main" would compare the PR against itself.
      Then every actions/cache step that can save, in a docs block containing "doctor-baseline", runs only on main

    Scenario: CI-14 the CI recipes install a bare rstest, as their pin tip says
      Given the "!!! tip" section of "docs/_snippets/ci-pin-tip.md"
      Then that docs section contains "The recipes use a bare `pip install rstest`"
      And no fenced block in "docs/guides/ci-*.md" contains "rstest=="
      And no fenced block in "docs/guides/sharding.md" contains "rstest=="

    Scenario: CI-14 the action README leaves PR suites to the actions-cache backend
      # actions-cache saves a PR-scoped cache on pull_request (cache-push auto),
      # so the artifact backend is for shard matrices, not PR suites.
      Given the table row of ".github/actions/rstest/README.md" starting with "| `cache-push`"
      Then that docs section contains "`actions-cache` saves only on `push`/`pull_request`"
      Given the table row of ".github/actions/rstest/README.md" starting with "| `artifact`"
      Then that docs section does not contain "PR"

    Scenario: CI-14 an artifact uploaded from .rstest_cache is not silently empty
      # upload-artifact v4.4+ drops hidden files and directories unless told
      # otherwise, and `if-no-files-found: ignore` hides the empty artifact, so
      # a failed run's replay journal would never reach the developer.
      Then every actions/upload-artifact step in the docs that uploads from a dot-directory sets include-hidden-files

    Scenario: CI-14 the warm-run lookup has one source
      # Copies of this step drifted apart before; the docs include the snippet,
      # and the action README (rendered by GitHub, no includes) must match it.
      Then no fenced block in "docs/**/*.md" contains "gh run list"
      And the ".github/actions/rstest/README.md" step that runs "gh run list" is the step in "docs/_snippets/warm-run-step.md"

    Scenario: CI-14 each provider's basic recipe reads first; its sharded variants are folded
      # ci-recipes.md is over a thousand lines; a GitLab reader should see the
      # GitLab basics without scrolling past every provider's shard matrix.
      Then no paragraph of "docs/guides/ci-recipes.md" matches "^\*\*(Sharding|Shared cache)"

    Scenario: CI-14 the guides index names every CI system the recipes page covers
      Then the "docs/guides/index.md" entry for "ci-recipes.md" names every level-2 heading of it except "Go deeper"

  Rule: --stream-json is a live side channel (docs/guides/ci-output.md)

    Scenario: CI-15 --stream-json writes live per-phase events and one closing sessionfinish
      Given a file "tests/test_api.py" containing:
        """
        def test_get():
            assert 200 == 200

        def test_post():
            print('posting')
            assert 201 == 200
        """
      When I run "rstest -n 2 --stream-json out/events.ndjson"
      Then the exit code is 1
      And stdout contains "1 failed, 1 passed"
      And the event stream "out/events.ndjson" has 6 "testreport" lines
      And the event stream "out/events.ndjson" reports "tests/test_api.py::test_post" call as "failed"
      And the last line of the event stream "out/events.ndjson" is a sessionfinish with exitstatus 1

    Scenario: CI-14 the warm-run lookup runs under bash on every runner OS
      # Windows runners default to PowerShell, where this bash step fails and
      # continue-on-error hides it: every run would start cold.
      Then every step in "docs/_snippets/warm-run-step.md" that runs a script sets "shell: bash"

    Scenario: CI-15 under a passthrough flag --stream-json still streams test reports, with no sessionfinish
      Given a file "tests/test_one.py" containing:
        """
        def test_one():
            pass
        """
      When I run "rstest -s --stream-json events.ndjson"
      Then the exit code is 0
      And the event stream "events.ndjson" has 3 "testreport" lines
      And the event stream "events.ndjson" has 0 "sessionfinish" lines
