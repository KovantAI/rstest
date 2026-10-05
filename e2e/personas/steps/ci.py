"""Steps for features/ci.feature: the CI / platform engineer."""

import contextlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import textwrap
import time
import xml.etree.ElementTree as ET
from pathlib import Path
from types import SimpleNamespace

import pytest
from _harness import REPO, find_python, git_init_commit, venv_bin
from pytest_bdd import given, parsers, then, when

from steps.common import q

DOCS = REPO / "docs"
ACTION = REPO / ".github" / "actions" / "rstest" / "action.yml"
SCHEMA = DOCS / "reference" / "schemas" / "report-json.schema.json"
CURSOR_RE = re.compile(r"\x1b\[\d*[ABCDJK]|\x1b\[\?25[lh]")
SGR_RE = re.compile(r"\x1b\[[\d;]*m")


# -- helpers -----------------------------------------------------------------


def _load(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _subst(world, text):
    """Expand {project}, {tmp} and {remote} placeholders in step text.
    Forward slashes: the text goes through shlex.split, which eats Windows
    backslashes."""
    for key, value in (
        ("project", world.project),
        ("tmp", world.gate.tmp),
        ("remote", world.notes.get("remote", "{remote}")),
    ):
        if isinstance(value, Path):
            value = value.as_posix()
        text = text.replace("{" + key + "}", str(value))
    return text


def _env_spec(world, spec):
    """`K=V K2=V2` (shell-quoted) as a dict, placeholders expanded."""
    return dict(item.split("=", 1) for item in shlex.split(_subst(world, spec)))


def _base_env(world, extra=None, drop=()):
    """The environment Gate.run builds, minus CI and color variables, for
    drivers that need their own process handling (pty, signals, shells)."""
    g = world.gate
    env = dict(os.environ, VIRTUAL_ENV=str(g.venv), RSTEST_WORKER_PATH=str(REPO / "python"))
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
        "CI",
        "GITHUB_ACTIONS",
        "NO_COLOR",
        "FORCE_COLOR",
        "PY_COLORS",
    ):
        env.pop(k, None)
    env.update(extra or {})
    for k in drop:
        env.pop(k, None)
    return env


def _write_exec(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    path.chmod(0o755)
    return path


def _purelib(py):
    return subprocess.run(
        [str(py), "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def _bare_venv(path):
    """A venv with no packages at all (no pip). Returns (python, purelib)."""
    subprocess.run([find_python(), "-m", "venv", "--without-pip", str(path)], check=True)
    py = venv_bin(path, "python")
    return py, _purelib(py)


def _run_pty(cmd, cwd, env, timeout=30):
    """Run cmd on a pseudo-terminal; return (exit code, decoded output)."""
    import pty
    import select

    m, s = pty.openpty()
    p = subprocess.Popen(
        cmd, cwd=cwd, env=env, stdin=s, stdout=s, stderr=s, close_fds=True, start_new_session=True
    )
    os.close(s)
    buf, deadline = b"", time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([m], [], [], 0.2)
            if ready:
                try:
                    data = os.read(m, 65536)
                except OSError:
                    break
                if not data:
                    break
                buf += data
            elif p.poll() is not None:
                break
    finally:
        if p.poll() is None:
            os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        os.close(m)
    return p.returncode, buf.decode("utf-8", "replace")


# -- doc parsing -------------------------------------------------------------


def _doc_blocks(path):
    """Fenced code blocks in a markdown file, as dedented strings. Handles
    fences indented under list items."""
    blocks, cur, indent = [], None, 0
    for ln in path.read_text(encoding="utf-8").splitlines():
        s = ln.lstrip()
        if cur is None:
            if s.startswith("```"):
                cur, indent = [], len(ln) - len(s)
        elif s.startswith("```"):
            blocks.append("\n".join(x[indent:] if x[:indent].isspace() else x for x in cur))
            cur = None
        else:
            cur.append(ln)
    return blocks


def _doc_block(path, *needles):
    """The first fenced block in `path` containing every needle, or None."""
    for b in _doc_blocks(path):
        if all(n in b for n in needles):
            return b
    return None


def _yaml_literal(text, key_re):
    """Body of the first `key: |` literal scalar whose key line matches
    `key_re`: the following lines indented deeper than the key, dedented."""
    lines = text.splitlines()
    for i, ln in enumerate(lines):
        if re.match(key_re, ln) and ln.rstrip().endswith("|"):
            # Column of the key itself, past any `- ` sequence marker.
            ind = len(ln) - len(ln.lstrip(" -"))
            body = []
            for x in lines[i + 1 :]:
                if x.strip() and len(x) - len(x.lstrip()) <= ind:
                    break
                body.append(x)
            return textwrap.dedent("\n".join(body)).strip("\n") + "\n"
    return None


def _yaml_list(text, key):
    """Items of the first `key:` block sequence (`- item` lines)."""
    lines = text.splitlines()
    for i, ln in enumerate(lines):
        if ln.strip() == f"{key}:":
            ind = len(ln) - len(ln.lstrip())
            items = []
            for x in lines[i + 1 :]:
                if x.strip() and len(x) - len(x.lstrip()) <= ind:
                    break
                if x.strip().startswith("- "):
                    items.append(x.strip()[2:])
            return items
    return None


# -- report checks -----------------------------------------------------------


def _schema_errors(doc, schema, node=None, path="$"):
    """Minimal draft-07 subset validator (type, required, properties,
    additionalProperties, items, $ref, allOf, minimum): enough for the
    report-json schema without a jsonschema dependency."""
    node = schema if node is None else node
    if "$ref" in node:
        ref = node["$ref"].split("/")[-1]
        return _schema_errors(doc, schema, schema["definitions"][ref], path)
    errs = []
    for sub in node.get("allOf", []):
        errs += _schema_errors(doc, schema, sub, path)
    types = node.get("type")
    if types:
        tmap = {
            "object": dict,
            "array": list,
            "string": str,
            "boolean": bool,
            "integer": int,
            "number": (int, float),
            "null": type(None),
        }
        tl = types if isinstance(types, list) else [types]
        ok = any(
            isinstance(doc, tmap[t]) and not (t in ("integer", "number") and isinstance(doc, bool))
            for t in tl
        )
        if not ok:
            return [*errs, f"{path}: expected {types}, got {type(doc).__name__}"]
    if "minimum" in node and isinstance(doc, (int, float)) and doc < node["minimum"]:
        errs.append(f"{path}: {doc} < {node['minimum']}")
    if isinstance(doc, dict):
        for k in node.get("required", []):
            if k not in doc:
                errs.append(f"{path}: missing {k}")
        props = node.get("properties", {})
        for k, v in doc.items():
            if k in props:
                errs += _schema_errors(v, schema, props[k], f"{path}.{k}")
            elif isinstance(node.get("additionalProperties"), dict):
                errs += _schema_errors(v, schema, node["additionalProperties"], f"{path}.{k}")
    if isinstance(doc, list) and isinstance(node.get("items"), dict):
        for i, v in enumerate(doc):
            errs += _schema_errors(v, schema, node["items"], f"{path}[{i}]")
    return errs


def _junit_summary(path):
    """(attribute counts, child-element counts, collect-error testcases) for
    the single <testsuite> in a JUnit file, or None if unreadable."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError):
        return None
    ts = root if root.tag == "testsuite" else root.find("testsuite")
    if ts is None:
        return None
    cases = list(ts.iter("testcase"))
    attrs = {k: int(ts.get(k, "0")) for k in ("tests", "failures", "errors", "skipped")}
    kids = {
        "tests": len(cases),
        "failures": sum(len(c.findall("failure")) for c in cases),
        "errors": sum(len(c.findall("error")) for c in cases),
        "skipped": sum(len(c.findall("skipped")) for c in cases),
    }
    collect = sum(
        1 for c in cases for e in c.findall("error") if e.get("message") == "collection failure"
    )
    return attrs, kids, collect


def _junit(world, path):
    ju = _junit_summary(world.project / path)
    assert ju is not None, f"{path} unreadable\n{world.tail()}"
    return ju


def _report(world, path):
    doc = _load(world.project / path)
    assert doc is not None, f"{path} unreadable\n{world.tail()}"
    return doc


# -- Given -------------------------------------------------------------------


@given("the project is a git repository with everything committed")
def _git_repo(world):
    git_init_commit(world.project)


@given(
    parsers.re(
        rf"(?P<count>\d+) files {q('pattern')} each holding (?P<n>\d+) trivial passing tests"
    )
)
def _trivial_files(world, count, pattern, n):
    src = "".join(f"def test_{i}():\n    pass\n\n" for i in range(int(n)))
    for f in range(int(count)):
        world.write(pattern.replace("{i}", str(f)), src)


@given(parsers.re(rf"a cache remote outside the project whose {q('name')} is a plain file"))
def _bad_remote(world, name):
    remote = world.gate.tmp / "ci_bad_remote"
    remote.mkdir(exist_ok=True)
    (remote / name).write_text("not a dir\n", encoding="utf-8")
    world.notes["remote"] = remote


@given("bash is on PATH")
def _bash(world):
    bash = shutil.which("bash")
    if bash is None:
        pytest.skip("no bash on PATH")
    world.notes["bash"] = bash


@given(
    parsers.re(
        rf"fake {q('tools')} CLIs that log their calls, and an rstest wrapper, first on PATH"
    )
)
def _fake_clis(world, tools):
    fake = world.gate.tmp / "ci_fakebin"
    log = world.gate.tmp / "ci_fake_calls.log"
    _write_exec(fake / "rstest", f'#!/bin/sh\nexec "{world.gate.binary}" "$@"\n')
    for tool in tools.split():
        _write_exec(fake / tool, f'#!/bin/sh\necho "{tool} $*" >> "{log}"\nexit 0\n')
    world.notes["fake_log"] = log
    world.notes["path"] = os.pathsep.join([str(fake), os.environ.get("PATH", "")])


@given(
    parsers.re(rf"the {q('key')} literal of the first block in {q('doc')} containing {q('needle')}")
)
def _snippet_literal(world, key, doc, needle):
    block = _doc_block(REPO / doc, needle)
    world.notes["snippet"] = block and _yaml_literal(block, r"\s*" + re.escape(key))


@given(parsers.re(rf"the first block in {q('doc')} containing {q('a')} and {q('b')}"))
def _snippet_block(world, doc, a, b):
    block = _doc_block(REPO / doc, a, b)
    world.notes["snippet"] = block and block + "\n"


@given(
    parsers.re(
        rf"the {q('key')} list of the first block in {q('doc')} containing {q('a')} and {q('b')}"
    )
)
def _snippet_list(world, key, doc, a, b):
    block = _doc_block(REPO / doc, a, b)
    lines = block and _yaml_list(block, key)
    world.notes["snippet"] = lines and "\n".join(lines) + "\n"


@given("REMOTE is an existing directory outside the project")
def _remote_dir(world):
    remote = world.gate.tmp / "ci_snip_remote"
    remote.mkdir(exist_ok=True)
    world.notes.setdefault("shell_env", {})["REMOTE"] = str(remote)


@given("REMOTE is a plain file outside the project")
def _remote_file(world):
    remote = world.gate.tmp / "ci_snip_remote_file"
    remote.write_text("x\n", encoding="utf-8")
    world.notes.setdefault("shell_env", {})["REMOTE"] = str(remote)


@given('the run script of the "Run rstest" step in .github/actions/rstest/action.yml')
def _action_step(world):
    lines = ACTION.read_text(encoding="utf-8").splitlines()
    step = None
    for i, ln in enumerate(lines):
        if ln.strip() == "- name: Run rstest":
            step = _yaml_literal("\n".join(lines[i:]), r"\s+run:")
            break
    world.notes["snippet"] = step
    # Every IN_* input the step reads defaults to empty, as on an unset input.
    env = dict.fromkeys(sorted(set(re.findall(r"\b(IN_[A-Z_]+)\b", step or ""))), "")
    env.update(
        {
            "MODE": "plain",
            "BACKEND": "none",
            "REMOTE": "",
            "CHANGED_REV": "",
            "CHANGED_MODE": "",
            "CACHE_PUSH": "false",
            "IN_CACHE_REMOTE_TOKEN": "",
        }
    )
    world.notes["action_env"] = env


@given("a fake rstest first on PATH that prints each argument as ARG<...> and exits with $FAKE_RC")
def _echo_rstest(world):
    echo_bin = world.gate.tmp / "ci_echobin"
    _write_exec(
        echo_bin / "rstest",
        '#!/bin/sh\nfor a in "$@"; do printf \'ARG<%s>\\n\' "$a"; done\nexit "${FAKE_RC:-0}"\n',
    )
    world.notes["echo_path"] = os.pathsep.join([str(echo_bin), os.environ.get("PATH", "")])


@given("the action step environment:")
def _action_env(world, docstring):
    for line in docstring.splitlines():
        name, value = line.split("=", 1)
        world.notes["action_env"][name] = value


@given(parsers.re(r"the action input (?P<name>IN_\w+) is padded with a space on each side"))
def _action_pad(world, name):
    world.notes.setdefault("unpadded", {})[name] = world.notes["action_env"][name]
    world.notes["action_env"][name] = f" {world.notes['action_env'][name]} "


@given(parsers.re(rf"a project .venv that sees the worker venv's packages and {q('deps')}"))
def _project_venv(world, deps):
    gate_purelib = _purelib(venv_bin(world.gate.venv, "python"))
    py, purelib = _bare_venv(world.project / ".venv")
    with open(f"{purelib}/ci_paths.pth", "w", encoding="utf-8") as f:
        f.write(gate_purelib + "\n" + str(world.project / deps) + "\n")
    world.notes["project_python"] = py


@given("a pre-commit hook env that sees only the worker venv's packages")
def _hook_env(world):
    gate_purelib = _purelib(venv_bin(world.gate.venv, "python"))
    hook = world.gate.tmp / "ci_hook_env"
    _, purelib = _bare_venv(hook)
    with open(f"{purelib}/ci_gate.pth", "w", encoding="utf-8") as f:
        f.write(gate_purelib + "\n")
    world.notes["hook"] = hook


@given(parsers.re(rf"the level-2 section {q('heading')} of {q('doc')}"))
def _doc_section(world, heading, doc):
    text = (REPO / doc).read_text(encoding="utf-8")
    world.notes["section"] = text.split(heading, 1)[-1].split("\n## ", 1)[0]


# -- When --------------------------------------------------------------------


@when(parsers.re(rf"I run {q('command')} with CI environment {q('env')}"))
def _run_env(world, command, env):
    world.run(_subst(world, command), env_extra=_env_spec(world, env))


@when(parsers.re(rf"I run {q('command')} in {q('subdir')} with CI environment {q('env')}"))
def _run_in_env(world, command, subdir, env):
    world.run(_subst(world, command), cwd=world.project / subdir, env_extra=_env_spec(world, env))


@when(parsers.re(rf"I run {q('command')} against that remote"))
def _run_remote(world, command):
    world.run(_subst(world, command))


def _plain_pytest(world, args, extra=None):
    py = venv_bin(world.gate.venv, "python")
    world.result = subprocess.run(
        [str(py), "-m", "pytest", *shlex.split(args)],
        cwd=world.project,
        env=_base_env(world, extra),
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=60,
    )


@when(parsers.re(rf"I run {q('args')} under plain pytest from the worker venv"))
def _run_pytest(world, args):
    _plain_pytest(world, args)


@when(
    parsers.re(
        rf"I run {q('args')} under plain pytest from the worker venv with CI environment {q('env')}"
    )
)
def _run_pytest_env(world, args, env):
    _plain_pytest(world, args, _env_spec(world, env))


@when(parsers.re(rf"I run {q('command')} and SIGINT its process group after (?P<secs>\d+) seconds"))
def _run_sigint(world, command, secs):
    argv = shlex.split(command)
    p = subprocess.Popen(
        [str(world.gate.binary), *argv[1:]],
        cwd=world.project,
        env=_base_env(world),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        time.sleep(float(secs))
        os.killpg(p.pid, signal.SIGINT)
        out, err = p.communicate(timeout=30)
    except subprocess.TimeoutExpired:
        out, err = "", "timed out after SIGINT"
    finally:
        with contextlib.suppress(OSError):
            os.killpg(p.pid, signal.SIGKILL)
        p.wait()
    world.result = SimpleNamespace(returncode=p.returncode, stdout=out, stderr=err)


def _run_shards(world, command, snapshot):
    cache = world.project / ".rstest_cache"
    if snapshot is not None:
        shutil.rmtree(snapshot, ignore_errors=True)
        shutil.copytree(cache, snapshot)
    shards = []
    for k in range(1, 5):
        shutil.rmtree(cache, ignore_errors=True)
        if snapshot is not None:
            shutil.copytree(snapshot, cache)  # every job restores one snapshot
        rp = world.gate.tmp / f"ci_shard.{k}.json"
        rp.unlink(missing_ok=True)
        r = world.run(f"{command} --shard {k}/4 --report-json {shlex.quote(str(rp))}")
        shards.append((r.returncode, rp))
    world.notes["shards"] = shards


@when(parsers.re(rf"I run {q('command')} as shards 1/4 to 4/4, each from a cold cache"))
def _shards_cold(world, command):
    _run_shards(world, command, None)


@when(
    parsers.re(rf"I run {q('command')} as shards 1/4 to 4/4, each from a copy of the current cache")
)
def _shards_warm(world, command):
    _run_shards(world, command, world.gate.tmp / "ci_shard_snapshot")


@when("I run shard-verify over the shard reports")
def _shard_verify(world):
    world.result = world.gate.run(
        "shard-verify", *(str(p) for _, p in world.notes["shards"]), cwd=world.project
    )


@when(parsers.re(rf"I run {q('command')} with BUILDKITE=true and no buildkite-agent on PATH"))
def _run_buildkite(world, command):
    empty = world.gate.tmp / "ci_empty_bin"
    empty.mkdir(exist_ok=True)
    path = os.pathsep.join([str(empty), "/usr/bin", "/bin"])
    world.run(command, env_extra={"BUILDKITE": "true", "PATH": path})


@when(parsers.re(rf"the agent expands {q('a')} and {q('b')} in the snippet to {q('value')}"))
def _expand_macros(world, a, b, value):
    world.notes["snippet"] = world.notes["snippet"].replace(a, value).replace(b, value)


@when(parsers.re(rf"I prefix the snippet with {q('line')}"))
def _prefix_snippet(world, line):
    world.notes["snippet"] = line + "\n" + world.notes["snippet"]


def _sh(world, script, shell, extra, cwd, path):
    f = world.gate.tmp / f"ci_snip_{abs(hash(script)) % 10**8}.sh"
    f.write_text(script, encoding="utf-8")
    argv = shlex.split(shell)
    assert argv[0] == "bash", shell
    world.result = subprocess.run(
        [world.notes["bash"], *argv[1:], str(f)],
        cwd=cwd,
        env=_base_env(world, {"PATH": path, **extra}),
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=120,
    )


@when(parsers.re(rf"I run the snippet with {q('shell')} in the suite"))
def _run_snippet(world, shell):
    extra = world.notes.get("shell_env", {})
    _sh(world, world.notes["snippet"], shell, extra, world.project, world.notes["path"])


@when(
    parsers.re(rf"I run the snippet with {q('shell')} in the suite with CI environment {q('env')}")
)
def _run_snippet_env(world, shell, env):
    extra = {**world.notes.get("shell_env", {}), **_env_spec(world, env)}
    _sh(world, world.notes["snippet"], shell, extra, world.project, world.notes["path"])


@when("I run the action step")
def _run_action(world):
    out = world.gate.tmp / "ci_gh_output"
    out.write_text("", encoding="utf-8")
    world.notes["gh_output"] = out
    extra = {**world.notes["action_env"], "GITHUB_OUTPUT": str(out)}
    _sh(
        world,
        world.notes["snippet"],
        "bash --noprofile --norc -eo pipefail",
        extra,
        world.gate.tmp,
        world.notes["echo_path"],
    )
    world.notes["argv"] = re.findall(r"^ARG<(.*)>$", world.result.stdout, re.M)


def _pty(world, command, extra=None):
    argv = [str(world.gate.binary), *shlex.split(command)[1:]]
    env = _base_env(world, {"TERM": "xterm", **(extra or {})})
    rc, out = _run_pty(argv, world.project, env)
    world.result = SimpleNamespace(returncode=rc, stdout=out, stderr="")


@when(parsers.re(rf"I run {q('command')} on a pseudo-terminal"))
def _run_on_pty(world, command):
    _pty(world, command)


@when(parsers.re(rf"I run {q('command')} on a pseudo-terminal with CI environment {q('env')}"))
def _run_on_pty_env(world, command, env):
    _pty(world, command, _env_spec(world, env))


@when(parsers.re(rf"I run {q('command')} with VIRTUAL_ENV set to the hook env"))
def _run_hook(world, command):
    world.run(command, env_extra={"VIRTUAL_ENV": str(world.notes["hook"])})


@when(
    parsers.re(
        rf"I run {q('command')} with VIRTUAL_ENV set to the hook env"
        r" and --python pointing at the project \.venv"
    )
)
def _run_hook_python(world, command):
    world.run(
        f"{command} --python {shlex.quote(str(world.notes['project_python']))}",
        env_extra={"VIRTUAL_ENV": str(world.notes["hook"])},
    )


# -- Then: exit code and streams ---------------------------------------------


@then(parsers.re(r"the exit code is neither (?P<a>\d+) nor (?P<b>\d+)"))
def _exit_neither(world, a, b):
    assert world.result.returncode not in (int(a), int(b)), world.tail()


@then("stderr contains the GITHUB_STEP_SUMMARY path")
def _stderr_summary(world):
    summary = (world.gate.tmp / "ci_nonexistent_dir" / "summary.md").as_posix()
    assert summary in world.result.stderr, world.tail()


@then(parsers.re(rf"a stdout line starts with {q('prefix')}"))
def _line_starts(world, prefix):
    lines = world.result.stdout.splitlines()
    assert any(ln.startswith(prefix) for ln in lines), world.tail()


@then(parsers.re(rf"a stdout line starts with {q('a')} or {q('b')}"))
def _line_starts_either(world, a, b):
    lines = world.result.stdout.splitlines()
    assert any(ln.startswith(a) or ln.startswith(b) for ln in lines), world.tail()


@then(parsers.re(rf"a stdout line starts with {q('prefix')} and contains {q('a')} or {q('b')}"))
def _line_starts_contains(world, prefix, a, b):
    lines = world.result.stdout.splitlines()
    assert any(ln.startswith(prefix) and (a in ln or b in ln) for ln in lines), world.tail()


def _lines_with(world, prefix):
    return [ln for ln in world.result.stdout.splitlines() if ln.startswith(prefix)]


@then(parsers.re(rf"stdout has exactly (?P<n>\d+) lines starting with {q('prefix')}"))
def _n_lines(world, n, prefix):
    assert len(_lines_with(world, prefix)) == int(n), world.tail()


@then(parsers.re(rf"a stdout line starting with {q('prefix')} contains {q('text')}"))
def _some_line_contains(world, prefix, text):
    assert any(text in ln for ln in _lines_with(world, prefix)), world.tail()


@then(parsers.re(rf"every stdout line starting with {q('prefix')} contains {q('text')}"))
def _every_line_contains(world, prefix, text):
    assert all(text in ln for ln in _lines_with(world, prefix)), world.tail()


@then("the output has cursor-movement sequences")
def _cursor(world):
    assert CURSOR_RE.search(world.output) is not None, world.tail()


@then("the output has no cursor-movement sequences")
def _no_cursor(world):
    found = sorted(set(CURSOR_RE.findall(world.output)))
    assert not found, f"cursor={found}\n{world.tail()}"


@then("the output has no SGR color codes")
def _no_sgr(world):
    found = sorted(set(SGR_RE.findall(world.output)))
    assert not found, f"sgr={found}\n{world.tail()}"


@then("stdout has SGR color codes")
def _sgr(world):
    assert SGR_RE.search(world.result.stdout) is not None, world.tail()


# -- Then: files and reports -------------------------------------------------


@then(parsers.re(rf"{q('path')} is a non-empty file"))
def _non_empty_file(world, path):
    p = world.project / path
    assert p.is_file() and p.stat().st_size > 0, f"{path} missing or empty"


@then(parsers.re(rf"report-json {q('path')} records meta.exitstatus (?P<code>\d+)"))
def _exitstatus(world, path, code):
    doc = _load(world.project / path)
    try:
        es = doc["meta"]["exitstatus"]
    except (TypeError, KeyError):
        es = None
    assert es == int(code), f"exitstatus={es}\n{world.tail()}"


@then(parsers.re(rf"the report-json schema check finds at least (?P<n>\d+) errors in {q('path')}"))
def _schema_rejects(world, n, path):
    schema = json.loads(SCHEMA.read_text("utf-8"))
    errs = _schema_errors(_report(world, path), schema)
    assert len(errs) >= int(n), str(errs)


@then(parsers.re(rf"the JUnit report {q('junit')} and the report-json {q('report')} are readable"))
def _reports_readable(world, junit, report):
    _junit(world, junit)
    _report(world, report)


@then(
    parsers.re(
        rf"the exit code is non-zero exactly when {q('path')} has a failure or error element"
    )
)
def _rc_iff_junit(world, path):
    _, kids, _ = _junit(world, path)
    junit_bad = kids["failures"] + kids["errors"] > 0
    assert (world.result.returncode != 0) == junit_bad, f"junit={kids}\n{world.tail()}"


@then(
    parsers.re(
        rf"the exit code is non-zero exactly when {q('path')} counts a failure,"
        r" error or collect error"
    )
)
def _rc_iff_json(world, path):
    counts = _report(world, path)["meta"]["counts"]
    json_bad = counts["failed"] + counts["errors"] + counts["collect_errors"] > 0
    assert (world.result.returncode != 0) == json_bad, f"counts={counts}\n{world.tail()}"


@then(parsers.re(rf"report-json {q('path')} records the exit code as meta.exitstatus"))
def _exitstatus_is_rc(world, path):
    es = _report(world, path)["meta"]["exitstatus"]
    assert es == world.result.returncode, f"exitstatus={es}\n{world.tail()}"


@then(parsers.re(rf"the testsuite counts in {q('path')} equal its child elements"))
def _junit_counts(world, path):
    attrs, kids, _ = _junit(world, path)
    assert attrs == kids, f"attrs={attrs} children={kids}"


@then(parsers.re(rf"report-json {q('path')} matches the documented report-json schema"))
def _schema_ok(world, path):
    schema = json.loads(SCHEMA.read_text("utf-8"))
    errs = _schema_errors(_report(world, path), schema)
    assert not errs, str(errs[:3])


@then(parsers.re(rf"report-json {q('report')} counts the collect error once, as {q('junit')} does"))
def _collect_once(world, report, junit):
    doc = _report(world, report)
    _, _, collect = _junit(world, junit)
    n = doc["meta"]["counts"]["collect_errors"]
    assert n == collect == 1 and len(doc["collect_errors"]) == 1, (
        f"json={n} {doc['collect_errors']} junit={collect}"
    )


# -- Then: shards ------------------------------------------------------------


@then("every shard exits 0 or 5")
def _shards_rc(world):
    rcs = [rc for rc, _ in world.notes["shards"]]
    assert all(rc in (0, 5) for rc in rcs), f"rcs={rcs}"


@then("every shard's report-json is stamped with its own index in meta.shard.k")
def _shards_stamped(world):
    shards = world.notes["shards"]
    stamped = [(_load(p) or {}).get("meta", {}).get("shard", {}).get("k") for _, p in shards]
    assert stamped == list(range(1, len(shards) + 1)), f"stamped={stamped}"


def _shard_sizes(world, name="test_long"):
    """(test count, holds a test matching `name`) per shard report."""
    sizes = []
    for _, p in world.notes["shards"]:
        tests = (_load(p) or {}).get("tests", {})
        sizes.append((len(tests), any(name in t for t in tests)))
    return sizes


@then(parsers.re(r"the shard reports hold (?P<n>\d+) tests in total"))
def _shards_total(world, n):
    sizes = _shard_sizes(world)
    assert sum(k for k, _ in sizes) == int(n), f"sizes={sizes}"


@then(
    parsers.re(
        rf"at least (?P<m>\d+) shard reports lack {q('name')}, each holding between half and"
        r" twice the mean of (?P<total>\d+) tests over (?P<shards>\d+) shards"
    )
)
def _shards_balanced(world, m, name, total, shards):
    sizes = _shard_sizes(world, name)
    others = [n for n, has in sizes if not has]
    mean = int(total) / int(shards)
    assert len(others) >= int(m) and all(mean / 2 <= n <= mean * 2 for n in others), (
        f"sizes={sizes}"
    )


# -- Then: snippets and the action -------------------------------------------


@then(parsers.re(rf"the snippet contains {q('text')}"))
def _snippet_contains(world, text):
    snippet = world.notes.get("snippet")
    assert snippet and text in snippet, str(snippet)[:200]


@then(parsers.re(rf"the fake CLI log contains {q('text')}"))
def _fake_log(world, text):
    log = world.notes["fake_log"]
    assert log.is_file() and text in log.read_text(encoding="utf-8"), world.tail()


@then(parsers.re(rf"rstest received {q('flag')} followed by {q('value')}"))
def _argv_pair(world, flag, value):
    argv = world.notes["argv"]
    assert flag in argv and argv[argv.index(flag) + 1 : argv.index(flag) + 2] == [value], (
        f"argv={argv}"
    )


@then(parsers.re(rf"rstest received the argument {q('value')}"))
def _argv_has(world, value):
    assert value in world.notes["argv"], f"argv={world.notes['argv']}"


@then(parsers.re(rf"rstest received {q('flag')} values (?P<values>.+)"))
def _argv_values(world, flag, values):
    argv = world.notes["argv"]
    got = [argv[i + 1] for i, x in enumerate(argv) if x == flag]
    assert got == re.findall(r'"([^"]*)"', values), f"argv={argv}"


@then(parsers.re(rf"rstest received {q('flag')} followed by the unpadded (?P<name>IN_\w+) input"))
def _argv_unpadded(world, flag, name):
    argv = world.notes["argv"]
    got = argv[argv.index(flag) + 1] if flag in argv else None
    assert got == world.notes["unpadded"][name], f"got={got!r}\n{world.tail(120)}"


@then(parsers.re(rf"GITHUB_OUTPUT contains {q('text')}"))
def _gh_output(world, text):
    assert text in world.notes["gh_output"].read_text(encoding="utf-8"), world.tail()


# -- Then: workspace and interpreter -----------------------------------------


@then("the git working tree is clean, untracked files included")
def _tree_clean(world):
    st = subprocess.run(
        ["git", "status", "--porcelain", "-uall"],
        cwd=world.project,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    assert st == "", f"porcelain={st[:200]!r}"


@then(parsers.re(rf"the output names the hook env or contains {q('text')}"))
def _names_hook(world, text):
    out = world.output
    assert str(world.notes["hook"]) in out or text in out, world.tail(300)


@then(parsers.re(rf"that level-2 section contains {q('a')} or {q('b')}"))
def _section_contains(world, a, b):
    sec = world.notes["section"]
    assert a in sec or b in sec, sec[:200]
