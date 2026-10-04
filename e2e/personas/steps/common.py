"""Steps shared by every persona: lay out a project, run rstest, inspect the run.

Quoted step arguments never contain a double quote; use a docstring step when
the text needs one. In file layouts, `{i}` in a path pattern and its content is
replaced by the file's index (0-based).
"""

import json
import re
import shlex
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from pytest_bdd import given, parsers, then, when


def q(name):
    """A double-quoted step argument."""
    return rf'"(?P<{name}>[^"]*)"'


@dataclass
class World:
    """Per-scenario state shared between steps."""

    gate: object
    project: Path
    result: object = None
    workers: int | None = None
    notes: dict = field(default_factory=dict)

    @property
    def output(self):
        return self.result.stdout + self.result.stderr

    def tail(self, n=400):
        return f"rc={self.result.returncode}\n--- output tail ---\n{self.output[-n:]}"

    def write(self, relpath, content):
        path = self.project / relpath
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def run(self, command, cwd=None, **kw):
        argv = shlex.split(command)
        assert argv and argv[0] == "rstest", f"step commands start with 'rstest': {command!r}"
        self.result = self.gate.run(*argv[1:], cwd=cwd or self.project, **kw)
        return self.result

    def run_noting_workers(self, command, cwd=None, **kw):
        """Run with --report-json and record `meta.workers` (the banner is
        hidden under -q). Single-worker mode records 0 and counts as 1."""
        report = self.gate.tmp / "workers-report.json"
        report.unlink(missing_ok=True)
        self.run(f"{command} --report-json {shlex.quote(str(report))}", cwd=cwd, **kw)
        try:
            n = json.loads(report.read_text(encoding="utf-8"))["meta"]["workers"]
        except (OSError, ValueError, KeyError):
            self.workers = None
        else:
            self.workers = max(n, 1)


@pytest.fixture
def world(gate):
    project = gate.tmp / "project"
    project.mkdir()
    return World(gate=gate, project=project)


# -- Given: project layout ---------------------------------------------------


@given("an empty project")
def _empty_project(world):
    pass


@given(parsers.re(rf"a file {q('path')} containing:"))
def _file(world, path, docstring):
    world.write(path, docstring + "\n")


@given(parsers.re(rf"an empty file {q('path')}"))
def _empty_file(world, path):
    world.write(path, "")


@given(parsers.re(rf"(?P<count>\d+) files {q('pattern')} each containing:"))
def _files(world, count, pattern, docstring):
    for i in range(int(count)):
        world.write(pattern.replace("{i}", str(i)), docstring.replace("{i}", str(i)) + "\n")


@given(parsers.re(rf"the directory {q('path')}"))
def _directory(world, path):
    (world.project / path).mkdir(parents=True, exist_ok=True)


# -- When: run rstest --------------------------------------------------------


@given(parsers.re(rf"I have run {q('command')}"))
@when(parsers.re(rf"I run {q('command')}"))
def _run(world, command):
    world.run(command)


@when(parsers.re(rf"I run {q('command')} in {q('subdir')}"))
def _run_in(world, command, subdir):
    world.run(command, cwd=world.project / subdir)


@when(parsers.re(rf"I run {q('command')} and note the worker count"))
def _run_workers(world, command):
    world.run_noting_workers(command)


# -- Then: exit code ---------------------------------------------------------


@then("the run succeeds")
def _succeeds(world):
    assert world.result.returncode == 0, world.tail()


@then("the run fails")
def _fails(world):
    assert world.result.returncode != 0, world.tail()


@then(parsers.re(r"the exit code is (?P<code>\d+)"))
def _exit_code(world, code):
    assert world.result.returncode == int(code), world.tail()


# -- Then: output ------------------------------------------------------------


@then(parsers.re(rf"the output contains {q('text')}"))
def _output_contains(world, text):
    assert text in world.output, world.tail()


@then(parsers.re(rf"the output does not contain {q('text')}"))
def _output_lacks(world, text):
    assert text not in world.output, world.tail()


@then(parsers.re(rf"the output contains {q('text')} exactly once"))
def _output_once(world, text):
    n = world.output.count(text)
    assert n == 1, f"{text!r} appears {n} times\n{world.tail()}"


@then(parsers.re(rf"stdout contains {q('text')}"))
def _stdout_contains(world, text):
    assert text in world.result.stdout, world.tail()


@then(parsers.re(rf"stdout does not contain {q('text')}"))
def _stdout_lacks(world, text):
    assert text not in world.result.stdout, world.tail()


@then(parsers.re(rf"stdout contains {q('text')} exactly once"))
def _stdout_once(world, text):
    n = world.result.stdout.count(text)
    assert n == 1, f"{text!r} appears {n} times in stdout\n{world.tail()}"


@then(parsers.re(rf"stderr contains {q('text')}"))
def _stderr_contains(world, text):
    assert text in world.result.stderr, world.tail()


@then(parsers.re(rf"stderr does not contain {q('text')}"))
def _stderr_lacks(world, text):
    assert text not in world.result.stderr, world.tail()


@then(parsers.re(rf"the stdout line containing {q('marker')} matches {q('pattern')}"))
def _stdout_line_matches(world, marker, pattern):
    line = next((ln for ln in world.result.stdout.splitlines() if marker in ln), None)
    assert line is not None, f"no stdout line contains {marker!r}\n{world.tail()}"
    assert re.search(pattern, line), f"{line!r} does not match {pattern!r}"


@then(parsers.re(rf"the last line of stdout matches {q('pattern')}"))
def _last_line_matches(world, pattern):
    lines = world.result.stdout.strip().splitlines()
    last = lines[-1] if lines else ""
    assert re.search(pattern, last), f"{last!r} does not match {pattern!r}"


# -- Then: workers -----------------------------------------------------------


@then(parsers.re(r"the worker count is (?P<n>\d+)"))
def _workers_exact(world, n):
    assert world.workers == int(n), f"workers={world.workers}\n{world.tail(200)}"


@then(parsers.re(r"the worker count is at least (?P<n>\d+)"))
def _workers_at_least(world, n):
    assert world.workers is not None and world.workers >= int(n), (
        f"workers={world.workers}\n{world.tail(200)}"
    )


# -- Then: filesystem --------------------------------------------------------


@then(parsers.re(rf"{q('path')} is a directory"))
def _is_dir(world, path):
    assert (world.project / path).is_dir(), f"{path} is not a directory"


@then(parsers.re(rf"{q('path')} is a file"))
def _is_file(world, path):
    assert (world.project / path).is_file(), f"{path} is not a file"


@then(parsers.re(rf"{q('path')} does not exist"))
def _absent(world, path):
    assert not (world.project / path).exists(), f"{path} exists"
