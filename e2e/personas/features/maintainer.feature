Feature: Suite maintainer
  A tech lead owns suite health over months: reads `rstest --doctor`, turns
  its metrics into CI gates (`--doctor-fail-on`, `--durations-regress`,
  `--fail-on-leak`, `--cov-fail-under`), and handles flaky and
  order-dependent tests (reruns, quarantine, `replay`, `bisect`). The numbers
  must be right and every gate must fire exactly when its condition is true.
  Timing checks assert on ratios with wide margins, not on tight wall times.

  Rule: doctor counts the whole test protocol, fixtures included

    Background:
      # Time lives in a function fixture (0.25s setup + 0.1s teardown); the
      # call itself is 0.02s.
      Given a file "test_fx.py" containing:
        """
        import time, pytest

        @pytest.fixture
        def slow():
            time.sleep(0.25)
            yield
            time.sleep(0.1)

        @pytest.mark.parametrize('i', range(12))
        def test_fx(slow, i):
            time.sleep(0.02)
        """

    Scenario: MT-01 doctor numbers on a fixture-bound suite match the measured speedup
      When I run "rstest -n 0 --doctor --doctor-json d0.json" and time it as "serial"
      Then the run succeeds
      When I run "rstest -n 4 --doctor --doctor-json d4.json --doctor-fail-on parallel_efficiency<30" and time it as "pool"
      Then stdout contains "12 passed"
      And the "serial" run took at least 2.0 times as long as the "pool" run
      And the doctor realized speedup in "d4.json" is within 40% of the "serial"/"pool" wall-time ratio
      And the JSON file "d4.json" has "parallel_efficiency.efficiency_pct" > 50
      # SLOWEST FILES counts fixture time: >= 2s of the ~4.4s.
      And the JSON file "d4.json" has "slowest_files.0.total_seconds" >= 2.0
      And stdout contains "WAIT-BOUND"
      # 'parallel_efficiency<30' must not fire on a ~3.5x run.
      And the run succeeds

    Scenario: MT-13 a -n 0 doctor run talks about one worker, not zero or all
      When I run "rstest -n 0 --doctor --doctor-json d0.json"
      Then stdout does not contain "0 workers"
      And stdout contains "1 worker" or "single-worker"
      And the JSON file "d0.json" has "workers" >= 1
      And stdout does not contain "across all workers"

  Rule: CPU work in a child process is computing, not waiting

    # os.times() reports no child CPU on Windows.
    @posix_only
    Scenario: MT-08 subprocess CPU is not reported as wait-bound
      Given a file "test_cli.py" containing:
        """
        import subprocess, sys, pytest

        LOOP = 'n = 0\nfor i in range(15_000_000):\n    n += i\n'

        @pytest.mark.parametrize('i', range(2))
        def test_cli_cpu(i):
            subprocess.run([sys.executable, '-c', LOOP], check=True)
        """
      When I run "rstest -n 2 --doctor --doctor-json d.json"
      Then the run succeeds
      # Above the WAIT-BOUND display floor (>= 1s).
      And the JSON file "d.json" has "test_time_seconds" >= 1.0
      And the JSON file "d.json" has "wait_bound.wait_pct" < 70 or no such field
      And the "WAIT-BOUND" section of stdout does not contain "test_cli_cpu"

  Rule: --doctor-fail-on fires exactly when a condition is true

    Background:
      # One 0.8s sleep + 1.0s of CPU: wait ~44% of 1.8s, long pole 0.8s.
      Given a file "test_w.py" containing:
        """
        import time

        def _spin(sec):
            end = time.process_time() + sec
            while time.process_time() < end:
                pass

        def test_sleep():
            time.sleep(0.8)

        def test_cpu_a():
            _spin(0.5)

        def test_cpu_b():
            _spin(0.5)
        """

    Scenario: MT-02 -n 0: every true doctor-fail-on condition fires a breach line
      # Pool-only metrics (efficiency, speedup, imbalance) are left out: cli.md
      # documents them as not measured without a pool.
      When I run "rstest -n 0 -q" with these doctor gates:
        | condition              |
        | wall_seconds>0.5       |
        | test_time_seconds>1.0  |
        | cpu_time_seconds>0.5   |
        | tests>2                |
        | wait_seconds>0.5       |
        | wait_pct>10            |
        | long_pole_seconds>0.5  |
      Then the exit code is 1
      And stderr contains "doctor gate failures"
      And the doctor gate "wall_seconds>0.5" fires
      And the doctor gate "test_time_seconds>1.0" fires
      And the doctor gate "cpu_time_seconds>0.5" fires
      And the doctor gate "tests>2" fires
      And the doctor gate "wait_seconds>0.5" fires
      And the doctor gate "wait_pct>10" fires
      And the doctor gate "long_pole_seconds>0.5" fires

    Scenario: MT-02 -n 2: every true doctor-fail-on condition fires a breach line
      When I run "rstest -n 2 -q" with these doctor gates:
        | condition               |
        | wall_seconds>0.5        |
        | test_time_seconds>1.0   |
        | cpu_time_seconds>0.5    |
        | tests>2                 |
        | wait_seconds>0.5        |
        | wait_pct>10             |
        | workers>1               |
        | long_pole_seconds>0.5   |
        | parallel_efficiency<101 |
        | efficiency_pct<101      |
        | realized_speedup>0      |
        | imbalance_pct>=0        |
      Then the exit code is 1
      And stderr contains "doctor gate failures"
      And the doctor gate "wall_seconds>0.5" fires
      And the doctor gate "test_time_seconds>1.0" fires
      And the doctor gate "cpu_time_seconds>0.5" fires
      And the doctor gate "tests>2" fires
      And the doctor gate "wait_seconds>0.5" fires
      And the doctor gate "wait_pct>10" fires
      And the doctor gate "workers>1" fires
      And the doctor gate "long_pole_seconds>0.5" fires
      And the doctor gate "parallel_efficiency<101" fires
      And the doctor gate "efficiency_pct<101" fires
      And the doctor gate "realized_speedup>0" fires
      And the doctor gate "imbalance_pct>=0" fires

    Scenario: MT-04 a skipped condition is not reported as a passed one
      When I run "rstest -n 0 -q --doctor-fail-on parallel_efficiency<1 --doctor-fail-on tests>100"
      Then the run succeeds
      And stderr contains "not measured"
      And stderr does not contain "all 2 condition(s) passed"
      And the last line of stderr contains "skipped"

  Rule: a malformed --doctor-fail-on aborts before any test runs

    Scenario Outline: MT-03 '<condition>' is rejected before the run, naming '<named>'
      # rstest's own flag errors exit 1 (exit-codes.md), so "non-zero, nothing
      # ran" is the contract, not pytest's 4.
      Given a file "test_a.py" containing:
        """
        def test_a():
            pass
        """
      When I run "rstest -n 2 --doctor-fail-on '<condition>'"
      Then the run fails
      And stdout does not contain " passed"
      And stderr contains "Error"
      And stderr contains "<named>"

      Examples:
        | condition        | named                  |
        | unknown_metric>1 | unknown_metric         |
        | tests>>1         | >1                     |
        |                  | no comparison operator |
        | wait_pct>NaN     | NaN                    |
        | tests!=inf       | inf                    |

  Rule: --durations-regress compares against the last good baseline

    Background:
      Given a file "test_p.py" containing:
        """
        import os, time

        def test_poll():
            assert not os.environ.get('MT_FAIL'), 'fails fast'
            time.sleep(float(os.environ.get('MT_D', '0.1')))

        def test_other():
            time.sleep(0.02)
        """

    Scenario: MT-05 a regression keeps firing until it is fixed
      # The gate must not adopt the regressed time as the new baseline.
      Given I have run "rstest -n 2 -q" with environment "MT_D=0.1"
      When I run "rstest -n 2 -q --durations-regress 2" with environment "MT_D=1.0"
      Then the exit code is 1
      And the output contains "test_poll"
      When I run "rstest -n 2 -q --durations-regress 2" with environment "MT_D=1.0"
      Then the exit code is 1
      And the output contains "test_poll"

    Scenario: MT-05 a failed run does not overwrite the baseline
      # A failed run's ~0s must not become the baseline.
      Given I have run "rstest -n 2 -q" with environment "MT_D=0.3"
      Then rstest explain "test_p.py::test_poll" reports a duration of at least 0.25s
      When I run "rstest -n 2 -q" with environment "MT_FAIL=1"
      Then the exit code is 1
      And rstest explain "test_p.py::test_poll" reports a duration of at least 0.25s
      When I run "rstest -n 2 -q --durations-regress 2" with environment "MT_D=1.0"
      Then the exit code is 1
      And the output contains "test_poll"

    Scenario: MT-05 a cold cache with --require-baseline refuses before any test runs
      # docs/guides/slowdowns.md: a dead gate is an error, not a silent pass.
      When I run "rstest -n 2 -q --durations-regress 2 --require-baseline --report-json r.json"
      Then the exit code is 1
      And stdout does not contain " passed"
      And stderr contains "--require-baseline"
      And "r.json" does not exist

    Scenario: MT-05 the regression row shows baseline -> current; the test session itself passed
      # docs/guides/slowdowns.md: exit 1 from the gate, meta.exitstatus 0.
      Given I have run "rstest -n 2 -q" with environment "MT_D=0.1"
      When I run "rstest -n 2 -q --durations-regress 2 --report-json r.json" with environment "MT_D=1.2"
      Then the exit code is 1
      And the stdout line containing "test_p.py::test_poll" matches "\d+\.\d\ds ->\s+\d+\.\d\ds"
      And stdout does not contain "test_other"
      And stderr contains "1 duration regression vs baseline"
      And the JSON file "r.json" has "meta.exitstatus" == 0

  Rule: --fail-on-leak blames the test that leaked

    # --dist loadfile keeps the file on one worker in file order, so the
    # warm-up test is the first one there (resource-leaks.md).

    Scenario Outline: MT-06a -n <n>: a module fixture's teardown does not hide the last test's leak
      # The module-scoped server's teardown runs inside the last test, which
      # also starts its own permanent thread.
      Given a file "test_srv.py" containing:
        """
        import threading, time, pytest

        @pytest.fixture(scope='module')
        def server():
            stop = threading.Event()
            t = threading.Thread(target=stop.wait, daemon=True)
            t.start()
            yield
            stop.set()
            t.join()

        def test_warmup():
            pass

        def test_uses_server(server):
            pass

        def test_last_leaks(server):
            threading.Thread(target=time.sleep, args=(60,), daemon=True).start()
        """
      When I run "rstest -n <n> -q --fail-on-leak <dist>"
      Then the exit code is 1
      And stderr contains "test_last_leaks"
      And stderr does not contain "test_uses_server"

      Examples:
        | n | dist            |
        | 0 |                 |
        | 2 | --dist loadfile |

    Scenario Outline: MT-06b -n <n>: a thread ending late does not net out a new permanent one
      # test_b's thread ends during test_c (made deterministic with an event
      # + join) while test_c starts a permanent one: net 0 must not hide it.
      Given a file "test_late.py" containing:
        """
        import threading, time

        GO = threading.Event()
        T = []

        def test_a_warmup():
            pass

        def test_b_short_thread():
            T.append(threading.Thread(target=GO.wait, daemon=True))
            T[0].start()

        def test_c_permanent():
            GO.set()
            T[0].join()
            threading.Thread(target=time.sleep, args=(60,), daemon=True).start()
        """
      When I run "rstest -n <n> -q --fail-on-leak <dist>"
      Then the exit code is 1
      And stderr contains "test_c_permanent"

      Examples:
        | n | dist            |
        | 0 |                 |
        | 2 | --dist loadfile |

    Scenario Outline: MT-07 -n <n>: capfd/asyncio.run/executor/subprocess pass the leak gate
      # Two files so -n 2 puts one file per worker; each starts with an
      # unchecked warm-up test.
      Given a file "test_io.py" containing:
        """
        import subprocess, sys

        def test_0_warmup():
            pass

        def test_capfd(capfd):
            print('hello')
            sys.stderr.write('err\n')
            assert capfd.readouterr() == ('hello\n', 'err\n')

        def test_subprocess_run():
            r = subprocess.run(
                [sys.executable, '-c', 'print(1)'], capture_output=True, text=True
            )
            assert r.stdout.strip() == '1'

        def test_tmp_file(tmp_path):
            with open(tmp_path / 'f.txt', 'w') as fh:
                fh.write('x')
            assert (tmp_path / 'f.txt').read_text() == 'x'
        """
      And a file "test_conc.py" containing:
        """
        import asyncio
        from concurrent.futures import ThreadPoolExecutor

        def test_0_warmup():
            pass

        def test_asyncio_run():
            async def f():
                await asyncio.sleep(0.01)
                return 1
            assert asyncio.run(f()) == 1

        def test_thread_pool_with():
            with ThreadPoolExecutor(max_workers=4) as ex:
                assert sum(ex.map(lambda x: x * 2, range(10))) == 90
        """
      When I run "rstest -n <n> -q --fail-on-leak --dist loadfile"
      Then the run succeeds
      And stderr contains "no thread/fd leaks detected"

      Examples:
        | n |
        | 0 |
        | 2 |

    Scenario: MT-07 -n 0: a shut-down session-scoped server is not a leak
      # What a wider-than-function fixture's setup creates is never charged to
      # a test (resource-leaks.md).
      Given a file "conftest.py" containing:
        """
        import threading, pytest
        from http.server import HTTPServer, BaseHTTPRequestHandler

        @pytest.fixture(scope='session')
        def http_server():
            srv = HTTPServer(('127.0.0.1', 0), BaseHTTPRequestHandler)
            t = threading.Thread(target=srv.serve_forever, daemon=True)
            t.start()
            yield srv.server_address
            srv.shutdown()
            srv.server_close()
            t.join()
        """
      And a file "test_srv.py" containing:
        """
        import pytest

        def test_0_warmup():
            pass

        @pytest.mark.parametrize('i', range(2))
        def test_uses_server(http_server, i):
            assert http_server[1] > 0
        """
      When I run "rstest -n 0 -q --fail-on-leak"
      Then the run succeeds
      And stderr contains "no thread/fd leaks detected"

  Rule: --cov-fail-under gates in every report mode, with pytest-cov's numbers

    Background:
      Given an empty file "pkg/__init__.py"
      And a file "pkg/full.py" containing:
        """
        def double(x):
            return x * 2
        """
      And a file "pkg/calc.py" containing:
        """
        def add(a, b):
            return a + b


        def sub(a, b):
            return a - b


        def classify(n):
            if n < 0:
                return 'neg'
            if n == 0:
                return 'zero'
            return 'pos'


        def unused_one(x):
            y = x + 1
            return y


        def unused_two(x):
            return -x
        """
      And a file "tests/test_a.py" containing:
        """
        from pkg.calc import add, classify
        from pkg.full import double

        def test_add():
            assert add(1, 2) == 3

        def test_classify():
            assert [classify(n) for n in (-1, 0, 1)] == ['neg', 'zero', 'pos']

        def test_double():
            assert double(2) == 4
        """
      And a file "tests/test_b.py" containing:
        """
        from pkg.calc import sub

        def test_sub():
            assert sub(3, 1) == 2
        """

    Scenario: MT-09 setup: pytest-cov is the oracle at ~82%
      When I run "pytest -q -p no:cacheprovider --cov=pkg --cov-report=term" in the worker venv
      Then the exit code is 0
      And stdout has exactly one coverage TOTAL line, ending in " 82%"
      When I run "pytest -q -p no:cacheprovider --cov=pkg --cov-fail-under=95" in the worker venv
      Then the exit code is 1
      And stdout contains "FAIL Required test coverage" exactly once

    Scenario Outline: MT-09 -n <n>: --cov-fail-under=95 fails once in every report mode
      Given pytest-cov's coverage TOTAL lines for the project have been recorded
      When I run "rstest -n <n> --cov=pkg --cov-report= --cov-fail-under=95" and note its coverage TOTAL lines
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      When I run "rstest -n <n> --cov=pkg --cov-report=term --cov-fail-under=95" and note its coverage TOTAL lines
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      When I run "rstest -n <n> --cov=pkg --cov-report=term-missing:skip-covered --cov-fail-under=95" and note its coverage TOTAL lines
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      # The fully covered pkg/full.py is skipped. Basenames only: the report
      # uses native path separators.
      And the output does not contain "full.py"
      And the output contains "calc.py"
      When I run "rstest -n <n> --cov=pkg --cov-report=annotate --cov-fail-under=95" and note its coverage TOTAL lines
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      When I run "rstest -n <n> --cov=pkg --cov-report=xml --cov-fail-under=95" and note its coverage TOTAL lines
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      When I run "rstest -n <n> --cov=pkg --cov-report=term --cov-report=xml --cov-fail-under=95" and note its coverage TOTAL lines
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      And the noted coverage TOTAL lines are all pytest-cov's TOTAL line

      Examples:
        | n |
        | 0 |
        | 2 |

    Scenario Outline: MT-09 -n <n>: .coveragerc fail_under and show_missing apply without a flag
      Given a file ".coveragerc" containing:
        """
        [report]
        fail_under = 95
        show_missing = True
        """
      When I run "rstest -n <n> --cov=pkg --cov-report=term"
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      # show_missing: the Missing column lists calc.py's unused lines.
      And the output contains "18-19, 23"

      Examples:
        | n |
        | 0 |
        | 2 |

    Scenario Outline: MT-09 -n <n>: --cov-config with a non-default data_file
      Given pytest-cov's coverage TOTAL lines for the project have been recorded
      And a file "cov.cfg" containing:
        """
        [run]
        data_file = covdata/.coverage
        """
      When I run "rstest -n <n> --cov=pkg --cov-config=cov.cfg --cov-report=term --cov-fail-under=95"
      Then the exit code is 1
      And the output contains "FAIL Required test coverage" exactly once
      And the coverage TOTAL lines of this run are exactly pytest-cov's
      And the output does not contain "No data to report"

      Examples:
        | n |
        | 0 |
        | 2 |

  Rule: the pool schedules the way a maintainer relies on

    Scenario Outline: MT-10 -n <n>: @serial tests reuse a worker's session fixture
      # An expensive session fixture (a DB) used by parallel and @serial tests.
      # Serial tests run on a worker that already has the session, so they add
      # no set-ups. Each set-up and each test run drops a file in MT_LOG_DIR.
      Given a file "conftest.py" containing:
        """
        import os, uuid, pytest

        @pytest.fixture(scope='session')
        def db():
            w = os.environ.get('RSTEST_WORKER_ID') or 'main'
            p = os.path.join(os.environ['MT_LOG_DIR'], f'up.{w}.{uuid.uuid4().hex}')
            with open(p, 'w') as f:
                f.write(w)
            yield
        """
      And a file "test_db.py" containing:
        """
        import os, time, pytest

        def _ran(name):
            p = os.path.join(os.environ['MT_LOG_DIR'], f'ran.{name}')
            with open(p, 'w') as f:
                f.write(os.environ.get('RSTEST_WORKER_ID') or 'main')

        @pytest.mark.parametrize('i', range(6))
        def test_par(db, i):
            time.sleep(0.05)
            _ran(f'par{i}')

        @pytest.mark.serial
        @pytest.mark.parametrize('i', range(3))
        def test_serial(db, i):
            _ran(f'serial{i}')
        """
      When I run "rstest -n <n> -q test_db.py" with a fresh MT_LOG_DIR
      Then the run succeeds
      And 9 tests logged a run in MT_LOG_DIR
      And the logged "serial" tests all ran on one worker
      And the logged session set-ups equal the workers that ran a "par" test

      Examples:
        | n |
        | 0 |
        | 2 |

    Scenario: MT-14 with a warm cache, cached long poles go out first, one per worker
      Given a file "test_poles.py" containing:
        """
        import time, pytest

        @pytest.mark.parametrize('i', range(4))
        def test_slow(i):
            time.sleep(1.0)

        def test_fast():
            pass
        """
      And I have run "rstest -n 4 -q"
      When I run "rstest -n 4 -v --doctor --report-json r.json"
      Then the run succeeds
      And stdout contains "5 passed"
      And the report "r.json" has 4 "test_slow" tests
      And the "test_slow" tests in "r.json" each ran on a different worker
      # One long pole per worker, not two (two would be >= 2.0s of sleep).
      And the JSON file "r.json" has "meta.duration_seconds" < 1.95

  Rule: an order-dependent failure can be reproduced and pinned on its polluter

    Scenario: MT-11 replay and bisect pin a shuffled parallel-only failure
      # The victim collects before its polluter; they only clash when shuffled
      # onto the same worker in polluter-first order.
      Given an empty file "tests/__init__.py"
      And a file "tests/state.py" containing:
        """
        DEBUG = False
        """
      And a file "tests/test_report.py" containing:
        """
        from tests import state

        def test_totals():
            assert not state.DEBUG, 'debug left on by an earlier test'
        """
      And a file "tests/test_zz_debug.py" containing:
        """
        from tests import state

        def test_enable_debug():
            state.DEBUG = True
        """
      And 4 files "tests/test_fill{i}.py" each containing:
        """
        def test_f():
            pass

        def test_g():
            pass
        """
      When I try "rstest -n 2 -q" with --shuffle seeds 1 to 40 until one exits 1 naming "test_totals"
      Then a shuffle seed hit the failure
      When I run "rstest replay" 5 times, keeping each result
      Then every one of those runs exited 1 with "test_totals" in stdout
      When I run "rstest bisect tests/test_report.py::test_totals"
      Then the run succeeds
      And stdout contains "culprit"
      And stdout contains "test_zz_debug.py::test_enable_debug"

    Scenario: MT-11 docs: --shuffle points to rstest replay for an exact repro
      Given the "### `--shuffle[=SEED]`" section of "docs/reference/cli.md"
      Then that docs section contains "replay"
      And that docs section does not contain "--dist loadfile` to keep the repro stable"

  Rule: the flaky policy flags combine without losing a failure

    Background:
      # The quarantined failure collects first and the real failure last, so
      # -x / --maxfail sees the quarantined one first. test_flaky_once fails
      # its first attempt only; MT_MARK puts @flaky(reruns=2) on it.
      Given a file "tests/test_0_quar.py" containing:
        """
        def test_quarantined():
            assert False
        """
      And a file "tests/test_5_flaky.py" containing:
        """
        import os, pathlib, pytest

        _flaky = pytest.mark.flaky(reruns=2) if os.environ.get('MT_MARK') else (lambda f: f)

        @_flaky
        def test_flaky_once():
            marker = pathlib.Path(os.environ['MT_FLAKY_MARKER'])
            if not marker.exists():
                marker.write_text('attempted')
                assert False, 'first attempt fails'
        """
      And a file "tests/test_9_real.py" containing:
        """
        def test_always_fails():
            assert False, 'real failure'
        """
      And 3 files "tests/test_ok{i}.py" each containing:
        """
        import pytest

        @pytest.mark.parametrize('i', range(10))
        def test_ok(i):
            pass
        """
      And a file "q.txt" containing:
        """
        tests/test_0_quar.py::test_quarantined
        """

    Scenario Outline: MT-12 -n <n> --reruns 2 --quarantine accounts for all 33 tests
      When I run the flaky-policy suite with "rstest -n <n> -q --reruns 2 --quarantine q.txt"
      Then the exit code is 1
      And the policy report counts are "failed=1 flaky=1 quarantined=1 passed=30"

      Examples:
        | n |
        | 0 |
        | 2 |

    Scenario Outline: MT-12 -n <n> @flaky without --reruns is counted once as flaky
      When I run the flaky-policy suite with "rstest -n <n> -q --quarantine q.txt -k 'not always'" and the @flaky mark on
      Then the exit code is 0
      And the policy report counts are "flaky=1 passed=30 failed=0"

      Examples:
        | n |
        | 2 |
        | 0 |

    Scenario Outline: MT-12 -n 2 <stop> with reruns: the always-failing test still fails the run
      # The flaky first attempt must not stop the run.
      When I run the flaky-policy suite with "rstest -n 2 -q <stop> -k 'not quarantined'"
      Then the exit code is 1
      And the policy report counts at least 1 failed

      Examples:
        | stop                   |
        | -x --reruns 1          |
        | --maxfail 1 --reruns 2 |

    Scenario Outline: MT-12 -n <n> -x --quarantine: a quarantined failure does not trip -x
      # The real failure later still runs and fails the run.
      When I run the flaky-policy suite with "rstest -n <n> -q -x --quarantine q.txt -k 'not flaky'"
      Then the exit code is 1
      And the policy report counts are "failed=1"

      Examples:
        | n |
        | 2 |
        | 0 |
