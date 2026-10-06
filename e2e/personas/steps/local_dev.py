"""Steps for features/local_dev.feature: the daily local developer.

Interactive drivers (pty, watch, SIGINT) run under hard timeouts and always
kill the process group, so a hang fails a step instead of hanging the session.
"""

import contextlib
import json
import os
import queue
import re
import select
import shlex
import shutil
import signal
import subprocess
import threading
import time
from types import SimpleNamespace

from _harness import REPO, WINDOWS, git_init_commit, venv_bin
from pytest_bdd import given, parsers, then, when

from steps.common import q

_ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
_LASTFAILED = ".pytest_cache/v/cache/lastfailed"


def _env(world, extra=None):
    """The environment Gate.run builds, for the Popen / pty / oracle drivers."""
    env = dict(
        os.environ, VIRTUAL_ENV=str(world.gate.venv), RSTEST_WORKER_PATH=str(REPO / "python")
    )
    for k in (
        "PYTEST_ADDOPTS",
        "PYTEST_CURRENT_TEST",
        "PYTEST_VERSION",
        "GITHUB_STEP_SUMMARY",
        "BUILDKITE",
        "GITHUB_BASE_REF",
        "CI_MERGE_REQUEST_DIFF_BASE_SHA",
        "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
        "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        "NO_COLOR",
        "FORCE_COLOR",
        "PY_COLORS",
        # CI turns the live footer off; these scenarios model a local terminal.
        "CI",
    ):
        env.pop(k, None)
    env["TERM"] = "xterm-256color"
    if extra:
        env.update(extra)
    return env


def _env_assignments(text):
    return dict(kv.split("=", 1) for kv in shlex.split(text or ""))


def _rstest_argv(world, command):
    argv = shlex.split(command)
    assert argv and argv[0] == "rstest", f"step commands start with 'rstest': {command!r}"
    return [str(world.gate.binary), *argv[1:]]


def _read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _report_tests(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))["tests"]
    except (OSError, ValueError, KeyError):
        return None


def _log_rows(d, prefix):
    """Rows `<time> <a> <b>` from per-process log files, oldest first."""
    rows = []
    for p in d.glob(prefix + ".*"):
        for ln in p.read_text(encoding="utf-8").splitlines():
            t, a, b = ln.split(" ", 2)
            rows.append((float(t), a, b))
    return sorted(rows)


def _kill_group(pid):
    with contextlib.suppress(ProcessLookupError, PermissionError):
        os.killpg(pid, signal.SIGKILL)


def _set_output(world, returncode, out):
    """Expose a driver's combined output to the common output steps."""
    world.result = SimpleNamespace(returncode=returncode, stdout=out, stderr="")


# -- the pytest oracle -------------------------------------------------------


def _twin(world):
    return world.gate.tmp / "oracle-twin"


def _oracle(world, command, cwd):
    argv = shlex.split(command)
    assert argv and argv[0] == "pytest", f"oracle commands start with 'pytest': {command!r}"
    py = venv_bin(world.gate.venv, "python")
    world.notes["oracle"] = subprocess.run(
        [str(py), "-m", "pytest", *argv[1:]],
        cwd=cwd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        # pytest writes a pipe in the locale encoding (cp1252 on Windows).
        env=_env(world, {"PYTHONIOENCODING": "utf-8"}),
        timeout=60,
    )


@when(parsers.re(rf"the pytest oracle runs {q('command')}(?: in {q('subdir')})?"))
def _oracle_run(world, command, subdir):
    _oracle(world, command, world.project / (subdir or ""))


@then("the pytest oracle exits 0 and collects at least 1 test")
def _oracle_collects(world):
    o = world.notes["oracle"]
    world.notes["collected"] = {ln for ln in o.stdout.splitlines() if "::" in ln}
    assert o.returncode == 0 and world.notes["collected"], f"rc={o.returncode} {o.stdout[-200:]}"


@given("a pytest oracle twin of the project")
def _make_twin(world):
    shutil.copytree(world.project, _twin(world))


@when(parsers.re(rf"I delete {q('path')} from the project and its twin"))
def _delete_both(world, path):
    (world.project / path).unlink()
    (_twin(world) / path).unlink()


@when(parsers.re(rf"I add {q('path')} to the project and its twin, containing:"))
def _add_both(world, path, docstring):
    world.write(path, docstring + "\n")
    (_twin(world) / path).write_text(docstring + "\n", encoding="utf-8")


@when(parsers.re(rf"I run {q('command')} and the pytest oracle runs {q('oracle')} in the twin"))
def _run_lockstep(world, command, oracle):
    """Oracle first, then rstest; the order log is reset so it shows this
    run's dispatch only."""
    _oracle(world, oracle, _twin(world))
    for p in world.project.glob("order.*"):
        p.unlink()
    world.run(command)


@then(parsers.re(rf"the first test the order log recorded is not {q('nodeid')}"))
def _first_logged_not(world, nodeid):
    rows = _log_rows(world.project, "order")
    assert rows and rows[0][2] != nodeid, str(rows[:3])


@then("the pytest oracle twin wrote a lastfailed file")
def _twin_lastfailed(world):
    assert _read_json(_twin(world) / _LASTFAILED) is not None


@then("lastfailed matches the pytest oracle twin's")
def _lastfailed_matches(world):
    rs = _read_json(world.project / _LASTFAILED)
    py = _read_json(_twin(world) / _LASTFAILED)
    assert rs == py, f"rstest={rs} pytest={py}"


@then(parsers.re(rf"the pytest oracle twin's lastfailed lists {q('nodeid')}"))
def _twin_lastfailed_lists(world, nodeid):
    py = _read_json(_twin(world) / _LASTFAILED)
    assert nodeid in (py or {}), f"pytest={py}"


@then(parsers.re(rf"some worker's replay-journal assignment starts with {q('nodeid')}"))
def _journal_head(world, nodeid):
    journal = _read_json(world.project / ".rstest_cache" / "replay" / "latest.json") or {}
    assignment = journal.get("assignment") or []
    assert any(w and w[0] == nodeid for w in assignment), str([w[:2] for w in assignment])


# -- JSON report -------------------------------------------------------------


@then(
    parsers.re(rf"the JSON report {q('path')} lists exactly the tests the pytest oracle collected")
)
def _report_equals_oracle(world, path):
    expected = world.notes["collected"]
    got = _report_tests(world.project / path)
    got = set(got) if got is not None else set()
    assert got == expected, f"missing={expected - got} extra={got - expected}"


@then(parsers.re(rf"the JSON report {q('path')} lists exactly {q('a')} and {q('b')}"))
def _report_exactly_two(world, path, a, b):
    ran = set(_report_tests(world.project / path) or {})
    assert ran == {a, b}, f"ran={sorted(ran)}"


@then(parsers.re(rf"the JSON report {q('path')} lists (?P<n>\d+) tests, including {q('nodeid')}"))
def _report_count_including(world, path, n, nodeid):
    tests = _report_tests(world.project / path) or {}
    assert nodeid in tests and len(tests) == int(n), str(sorted(tests))


# -- git / filesystem --------------------------------------------------------


@given("the project is committed to a fresh git repository")
def _git_commit_project(world):
    git_init_commit(world.project)


@then("git status --porcelain -uall shows a clean tree")
def _porcelain_clean(world):
    out = subprocess.run(
        ["git", "status", "--porcelain", "-uall"],
        cwd=world.project,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    assert out == "", out[:300]


@when(parsers.re(rf"I remove {q('path')} if present"))
def _remove_if_present(world, path):
    (world.project / path).unlink(missing_ok=True)


# -- generic output ----------------------------------------------------------


@when(parsers.re(rf"I run {q('command')} with {q('env')}"))
def _run_with_env(world, command, env):
    world.run(command, env_extra=_env_assignments(env))


@then(parsers.re(rf"stdout matches {q('pattern')}"))
def _stdout_matches(world, pattern):
    assert re.search(pattern, world.result.stdout), world.tail()


@then(parsers.re(rf"the output matches {q('pattern')}"))
def _output_matches(world, pattern):
    assert re.search(pattern, world.output), world.tail()


# -- --incremental -----------------------------------------------------------


@when(
    parsers.re(rf"I run {q('command')}(?: in {q('subdir')})? after clearing stale \.coverage files")
)
def _run_incremental(world, command, subdir):
    unit = world.project / "tests" / "unit"
    for stale in [*world.project.glob(".coverage*"), *unit.glob(".coverage*")]:
        stale.unlink()
    world.run(command, cwd=world.project / (subdir or ""))


@then(parsers.re(r"the incremental outcomes record (?P<n>\d+) green tests"))
def _incremental_green(world, n):
    outcomes = _read_json(world.project / ".rstest_cache" / "incremental_outcomes.json") or {}
    green = sorted(outcomes.get("green") or [])
    assert len(green) == int(n), f"green={green}\n{world.tail()}"


@then("every key in the coverage index names a file under the project root")
def _index_rootdir_relative(world):
    index = _read_json(world.project / ".rstest_cache" / "coverage_index.json") or {}
    keys = sorted(index.get("files") or {})
    assert keys and all((world.project / k).is_file() for k in keys), str(keys)


# -- --watch -----------------------------------------------------------------


class _Watch:
    """`rstest --watch` driven over pipes; every wait has a deadline."""

    def __init__(self, argv, cwd, env):
        self.proc = subprocess.Popen(
            argv,
            cwd=str(cwd),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            start_new_session=not WINDOWS,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        for line in self.proc.stdout:
            self.lines.put(line)

    def wait_for(self, needle, timeout=30):
        buf, deadline = [], time.time() + timeout
        while time.time() < deadline:
            try:
                line = self.lines.get(timeout=0.25)
            except queue.Empty:
                continue
            buf.append(line)
            if needle in line:
                return True, "".join(buf)
        return False, "".join(buf)

    def cycle(self, edit, timeout=30):
        """Apply one edit and collect the output up to the next idle prompt.
        The idle prompt of the previous cycle has already been consumed."""
        time.sleep(0.5)
        edit()
        return self.wait_for("waiting for changes", timeout)

    def stop(self):
        try:
            if self.proc.poll() is None:
                self.proc.stdin.write("q\n")
                self.proc.stdin.flush()
                self.proc.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            pass
        finally:
            if self.proc.poll() is None:
                if WINDOWS:
                    self.proc.kill()
                else:
                    _kill_group(self.proc.pid)
                self.proc.wait(timeout=10)


def _watch_result(world, ok, out):
    world.notes["watch_idle"] = ok
    _set_output(world, None, out)


@when(
    parsers.re(rf"I start {q('command')} and wait up to (?P<secs>\d+) seconds for its idle prompt")
)
def _start_watch(world, request, command, secs):
    w = _Watch(_rstest_argv(world, command), world.project, _env(world))
    request.addfinalizer(w.stop)
    world.notes["watch"] = w
    _watch_result(world, *w.wait_for("waiting for changes", int(secs)))


@when(parsers.re(rf"I save {q('path')} while watching:"))
def _watch_save(world, path, docstring):
    w = world.notes["watch"]
    _watch_result(world, *w.cycle(lambda: world.write(path, docstring + "\n")))


@when(parsers.re(rf"I delete {q('path')} while watching"))
def _watch_delete(world, path):
    w = world.notes["watch"]
    _watch_result(world, *w.cycle(lambda: (world.project / path).unlink()))


@when(
    parsers.re(
        rf"I save {q('path')} while watching and give it (?P<secs>\d+) seconds to start a rerun:"
    )
)
def _watch_save_no_wait(world, path, docstring, secs):
    w = world.notes["watch"]
    time.sleep(0.5)
    world.write(path, docstring + "\n")
    rerun, out = w.wait_for("changed;", int(secs))
    world.notes["watch_rerun"] = rerun
    _set_output(world, None, out)


@then("the watcher printed its idle prompt")
def _watch_idle(world):
    assert world.notes["watch_idle"], world.output[-300:]


@then("the watcher did not start a rerun")
def _watch_no_rerun(world):
    assert not world.notes["watch_rerun"], world.output[-300:]


@then("the watcher is still running")
def _watch_running(world):
    assert world.notes["watch"].proc.poll() is None, world.output[-300:]


@then('the output shows no "Traceback" and no "error" in any case')
def _no_traceback_or_error(world):
    out = world.output
    assert "Traceback" not in out and "error" not in out.lower().replace("last exit: 0", ""), out[
        -300:
    ]


# -- pty ---------------------------------------------------------------------


def _pty_run(argv, cwd, env, feed=b"", when=b"(Pdb)", cols=120, timeout=30):
    """Run argv on a fresh pty (POSIX only). Writes `feed` once `when` shows
    up in the output. Returns (exit code or None on timeout, decoded output).
    The child is a session leader (pty.fork calls setsid), so killing its
    process group also takes down any worker it spawned."""
    import fcntl
    import pty
    import struct
    import termios

    pid, fd = pty.fork()
    if pid == 0:  # child
        try:
            os.chdir(str(cwd))
            os.execve(str(argv[0]), [str(a) for a in argv], env)
        finally:
            os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, cols, 0, 0))
    buf, fed, status = b"", not feed, None
    deadline = time.time() + timeout
    try:
        while time.time() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    chunk = b""
                if not chunk:
                    break
                buf += chunk
            if not fed and when in buf:
                os.write(fd, feed)
                fed = True
            done, st = os.waitpid(pid, os.WNOHANG)
            if done:
                status = st
                break
        while status is None and time.time() < deadline:
            done, st = os.waitpid(pid, os.WNOHANG)
            if done:
                status = st
                break
            time.sleep(0.05)
    finally:
        if status is None:
            _kill_group(pid)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, 0)
        os.close(fd)
    rc = os.waitstatus_to_exitcode(status) if status is not None else None
    return rc, buf.decode("utf-8", "replace")


@when(
    parsers.re(
        rf"I run {q('command')} on a(?: (?P<cols>\d+)-column)? pty(?: with {q('env')})?"
        rf"(?:, typing {q('feed')} at the \(Pdb\) prompt)?"
    )
)
def _run_on_pty(world, command, cols, env, feed):
    """The raw pty output (escapes included) becomes the run's stdout; a run
    killed at the 30 s timeout has returncode None."""
    rc, out = _pty_run(
        _rstest_argv(world, command),
        world.project,
        _env(world, _env_assignments(env)),
        feed=(feed + "\n").encode() if feed else b"",
        cols=int(cols) if cols else 120,
    )
    _set_output(world, rc, out)


@then("the pty run ended before its timeout")
def _pty_ended(world):
    assert world.result.returncode is not None, _ANSI.sub("", world.output)[-300:]


@then("the output outside the rstest banner shows a (Pdb) prompt or a hint naming -n 0 or -s")
def _pdb_or_hint(world):
    plain = _ANSI.sub("", world.output)
    # The banner itself mentions -n 0; only the rest of the output counts.
    body = "\n".join(ln for ln in plain.splitlines() if not ln.startswith("rstest "))
    assert "(Pdb)" in body or "-n 0" in body or " -s" in body, plain[-300:]


@then("the output contains ANSI escape sequences")
def _has_escapes(world):
    assert "\x1b[" in world.output, f"rc={world.result.returncode}"


@then("the output contains no escape character")
def _no_escapes(world):
    assert "\x1b" not in world.output, f"esc={world.output.count(chr(27))}"


def _footer(world):
    lines = (_ANSI.sub("", x) for x in re.split(r"\r\n|\n|\r", world.output))
    return [ln for ln in lines if re.search(r"\d+% \(\d+/\d+\)", ln) or re.match(r"gw\d+\s", ln)]


@then("the live footer was drawn")
def _footer_drawn(world):
    assert _footer(world), f"rc={world.result.returncode}"


@then(parsers.re(r"every live footer line fits in (?P<cols>\d+) columns"))
def _footer_fits(world, cols):
    wide = [ln for ln in _footer(world) if len(ln) > int(cols)]
    assert not wide, f"{len(wide)} wide, e.g. {wide[:1]}"


@then("if stdout has any escape sequence, its last line has one too")
def _color_all_or_nothing(world):
    out = world.result.stdout
    last = out.strip().splitlines()[-1] if out.strip() else ""
    assert "\x1b" not in out or "\x1b" in last, f"esc={out.count(chr(27))} summary={last!r}"


# -- tracebacks --------------------------------------------------------------


def _failure_body(text, header_re):
    """Lines of the single failure block, after its header, up to the
    captured-output section; leading blank lines dropped."""
    lines = text.splitlines()
    start = next((i for i, ln in enumerate(lines) if re.search(header_re, ln)), None)
    if start is None:
        return []
    body = []
    for ln in lines[start + 1 :]:
        if ln.startswith("---") or ln.startswith("==="):
            break
        body.append(ln)
    while body and not body[0].strip():
        body.pop(0)
    while body and not body[-1].strip():
        body.pop()
    return body


def _oracle_body(world, test):
    return _failure_body(world.notes["oracle"].stdout, rf"^_+ {re.escape(test)} _+$")


def _rstest_body(world, test):
    return _failure_body(world.result.stdout, rf"^--- FAILED .*{re.escape(test)} ---$")


@then(
    parsers.re(rf"the pytest oracle's failure body for {q('test')} has more than (?P<n>\d+) lines")
)
def _oracle_body_len(world, test, n):
    exp = _oracle_body(world, test)
    assert len(exp) > int(n), f"exp={exp}"


@then(
    parsers.re(
        rf"rstest's failure body for {q('test')} matches the pytest oracle's after the first line"
    )
)
def _body_tail_matches(world, test):
    exp, got = _oracle_body(world, test), _rstest_body(world, test)
    assert got[1:] == exp[1:], f"\nexp={exp}\ngot={got}"


@then(parsers.re(rf"rstest's failure body for {q('test')} matches the pytest oracle's exactly"))
def _body_matches(world, test):
    exp, got = _oracle_body(world, test), _rstest_body(world, test)
    assert got == exp, f"\nexp={exp}\ngot={got}"


@then(
    parsers.re(
        rf"the failure body for {q('test')} starts with {q('line')}"
        r" under both rstest and the pytest oracle"
    )
)
def _body_first_line_both(world, test, line):
    exp, got = _oracle_body(world, test), _rstest_body(world, test)
    assert got and got[0] == exp[0] == line, f"got={got[:1]} exp={exp[:1]}"


@then(
    parsers.re(
        rf"the last (?P<n>\d+) lines of rstest's failure body for {q('test')}"
        r" match the pytest oracle's"
    )
)
def _body_ends_match(world, n, test):
    n = int(n)
    exp, got = _oracle_body(world, test), _rstest_body(world, test)
    assert got[-n:] == exp[-n:], f"\nexp={exp[-n:]}\ngot={got[-n:]}"


@then(parsers.re(rf"rstest's failure body for {q('test')} starts with {q('line')}"))
def _body_starts(world, test, line):
    got = _rstest_body(world, test)
    assert got and got[0] == line, f"got={got[:1]}"


# -- -x and Ctrl-C -----------------------------------------------------------


def _fail_ts(world):
    rows = _log_rows(world.project, "starts")
    return rows, next((t for t, kind, _ in rows if kind == "fail"), None)


@then("the start log recorded the failure")
def _start_log_failure(world):
    assert _fail_ts(world)[1] is not None, world.tail(200)


@then(
    parsers.re(r"no test started more than (?P<secs>[\d.]+) seconds after the failure was logged")
)
def _no_late_start(world, secs):
    rows, fail_ts = _fail_ts(world)
    late = [(round(t - fail_ts, 3), i) for t, kind, i in rows if kind == "start" and fail_ts]
    late = [x for x in late if x[0] > float(secs)]
    assert not late, f"started after fail (+s, id): {late}"


@when(
    parsers.re(
        rf"I run {q('command')} and send SIGINT to its process group after (?P<secs>[\d.]+) seconds"
    )
)
def _run_and_interrupt(world, command, secs):
    proc = subprocess.Popen(
        _rstest_argv(world, command),
        cwd=str(world.project),
        env=_env(world),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        start_new_session=True,
    )
    try:
        time.sleep(float(secs))
        os.killpg(proc.pid, signal.SIGINT)
        try:
            out, _ = proc.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            _kill_group(proc.pid)
            out, _ = proc.communicate(timeout=10)
            out = "HANG after SIGINT\n" + out
    finally:
        if proc.poll() is None:
            _kill_group(proc.pid)
            proc.wait(timeout=10)
    _set_output(world, proc.returncode, out)


def _inflight(world):
    return set(re.findall(r"^\s+gw\d+\s+(\S+::\S+)", world.output, re.M))


@then("the output lists tests in flight on workers")
def _lists_inflight(world):
    assert _inflight(world), world.output[-300:]


@then("rstest exited within 30 seconds of the SIGINT")
def _no_hang(world):
    assert "HANG" not in world.output, world.output[-300:]


@then("lastfailed lists none of the in-flight tests")
def _inflight_not_lastfailed(world):
    lf = _read_json(world.project / _LASTFAILED) or {}
    hit = sorted(_inflight(world) & set(lf))
    assert not hit, f"in lastfailed: {hit}"


@then("flakes.json has no entry for any in-flight test")
def _inflight_not_flaky(world):
    flakes = _read_json(world.project / ".rstest_cache/flakes.json") or {}
    hit = sorted(_inflight(world) & set(flakes))
    assert not hit, f"in flakes.json: {hit}"


@then("durations.json has no 0.0 duration for any in-flight test")
def _inflight_no_zero_duration(world):
    durs = _read_json(world.project / ".rstest_cache/durations.json") or {}
    zero = sorted(n for n in _inflight(world) if n in durs and durs[n].get("secs") == 0.0)
    assert not zero, f"zero durations: {zero}"
