"""Steps for features/onboarding.feature: the first-time evaluator."""

import os
import re
import subprocess
import sys

from _harness import REPO, find_python, venv_bin
from pytest_bdd import given, parsers, then, when

from steps.common import q

_SEP = ";" if sys.platform == "win32" else ":"


def _purelib(py):
    return subprocess.run(
        [str(py), "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def _bare_project_venv(world):
    """A .venv with no packages at all (no pip), like a project venv rstest
    was never installed into. A .git dir bounds rstest's .venv walk. Returns
    the venv's purelib."""
    (world.project / ".git").mkdir(exist_ok=True)
    venv = world.project / ".venv"
    subprocess.run([find_python(), "-m", "venv", "--without-pip", str(venv)], check=True)
    return _purelib(venv_bin(venv, "python"))


@then("try does not silently report the suite as drop-in ready")
def _try_not_silent(world):
    out = world.result.stdout
    assert not ("drop-in ready" in out and "already red" not in out), world.tail()


@given("a project .venv that has the project's deps but not rstest")
def _venv_without_rstest(world):
    world.write("deps/onb_dep.py", "VALUE = 1\n")
    world.write(
        "tests/test_dep.py", "import onb_dep\n\ndef test_dep():\n    assert onb_dep.VALUE == 1\n"
    )
    purelib = _bare_project_venv(world)
    with open(f"{purelib}/onb_deps.pth", "w", encoding="utf-8") as f:
        f.write(str(world.project / "deps") + "\n")


@given("the worker interpreter is first on PATH")
def _worker_python_on_path(world):
    gate_bin = venv_bin(world.gate.venv, "python").parent
    world.notes["env"] = {"PATH": str(gate_bin) + _SEP + os.environ.get("PATH", "")}


@given("a usable project .venv")
def _usable_venv(world):
    world.write("tests/test_ok.py", "def test_ok():\n    pass\n")
    purelib = _bare_project_venv(world)
    # The worker venv's site-packages carry msgpack + pytest: the shim imports.
    with open(f"{purelib}/onb_gate.pth", "w", encoding="utf-8") as f:
        f.write(_purelib(venv_bin(world.gate.venv, "python")) + "\n")


@given("a .python-version pinning a different minor than the .venv")
def _stale_pin(world):
    minor = sys.version_info.minor
    world.write(".python-version", (f"3.{minor - 1}" if minor > 10 else f"3.{minor + 1}") + "\n")


@when(parsers.re(rf"I run {q('command')} with VIRTUAL_ENV unset"))
def _run_without_venv(world, command):
    world.run(command, env_extra=world.notes.get("env"), env_drop=("VIRTUAL_ENV",))


@then("the output mentions the missing dependency or that rstest is not installed in the .venv")
def _names_missing(world):
    out = world.output
    assert "onb_dep" in out or "rstest is not installed in it" in out, world.tail()


@then("the run succeeds, or the project .venv was rejected only for the version pin")
def _rejected_for_pin(world):
    out = world.output.replace("\\", "/")
    assert world.result.returncode == 0 or ("/.venv/" in out and "does not satisfy" in out), (
        world.tail()
    )


@then(parsers.re(rf"the run succeeds, or the output contains {q('text')}"))
def _succeeds_or_contains(world, text):
    assert world.result.returncode == 0 or text in world.output, world.tail()


# -- docs a newcomer reads ----------------------------------------------------

_WORDS = "zero one two three four five six seven eight nine ten eleven twelve"
_NUMBERS = {w: i for i, w in enumerate(_WORDS.split())}
_NUMBERS["single"] = 1
# "<number> [adjective] tests|test files|files|workers": "Twelve one-second
# tests", "one small file", "a single worker", "two test files".
_COUNT_RE = re.compile(
    rf"\b({'|'.join(_NUMBERS)}|\d+)\s+(?:[\w-]+\s+)?(test files?|tests?|files?|workers?)\b",
    re.IGNORECASE,
)


def _fences(text):
    """(language, body) of each fenced block, plus the prose between them:
    returns (blocks, prose) where prose[k] is the text before block k (and
    prose[-1] the text after the last block)."""
    blocks, prose, cur, lang = [], [[]], None, ""
    for ln in text.splitlines():
        s = ln.strip()
        if cur is None and s.startswith("```"):
            cur, lang = [], s[3:].strip()
        elif cur is not None and s.startswith("```"):
            blocks.append((lang, "\n".join(cur)))
            cur = None
            prose.append([])
        elif cur is not None:
            cur.append(ln)
        else:
            prose[-1].append(ln)
    return blocks, [" ".join(" ".join(p).split()) for p in prose]


@given(
    parsers.re(rf"the python blocks of {q('doc')} containing {q('needle')}, joined, as {q('path')}")
)
def _doc_python_blocks(world, doc, needle, path):
    blocks, _ = _fences((REPO / doc).read_text(encoding="utf-8"))
    code = [b for lang, b in blocks if lang == "python" and needle in b]
    assert code, f"no python block of {doc} contains {needle!r}"
    target = world.project / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("\n\n".join(code) + "\n", encoding="utf-8")


@given(parsers.re(rf"in {q('path')}, {q('old')} is replaced by {q('new')}"))
def _replace_in(world, path, old, new):
    target = world.project / path
    text = target.read_text(encoding="utf-8")
    assert old in text, f"{old!r} not in {path}"
    target.write_text(text.replace(old, new), encoding="utf-8")


@given(
    parsers.re(
        r"the prose before code block (?P<k>\d+) "
        rf"of the {q('heading')} section of {q('doc')}"
    )
)
def _doc_prose(world, k, heading, doc):
    text = (REPO / doc).read_text(encoding="utf-8")
    assert heading in text, f"{doc} has no {heading!r}"
    section = text.split(heading, 1)[1].split("\n## ", 1)[0]
    _, prose = _fences(section)
    # Code block numbering starts at 1: "before code block 1" is the intro.
    world.notes["prose"] = prose[int(k) - 1]


@then(
    parsers.re(
        r"every count that prose states matches (?P<tests>\d+) tests?, "
        r"(?P<files>\d+) files? and (?P<workers>\d+) workers?"
    )
)
def _prose_counts(world, tests, files, workers):
    prose = world.notes["prose"]
    want = {"test": int(tests), "file": int(files), "worker": int(workers)}
    claims = list(_COUNT_RE.finditer(prose))
    assert claims, f"no count in: {prose}"
    for m in claims:
        said, word, noun = m.group(0), m.group(1).lower(), m.group(2).lower()
        n = _NUMBERS[word] if word in _NUMBERS else int(word)
        kind = "file" if "file" in noun else noun.rstrip("s")
        assert n == want[kind], f"{said!r} but the walkthrough has {want[kind]} {kind}(s): {prose}"


def _nav_tree(text):
    """mkdocs.yml's `nav:` as nested [(title or None, path or children)]."""
    lines = text.split("\nnav:\n", 1)[1].splitlines()
    root = []
    stack = [(-1, root)]
    for ln in lines:
        if ln.strip() and not ln.startswith(" "):
            break
        m = re.match(r"(\s*)- (?:(.+?):\s*(\S*)|(\S+))\s*$", ln)
        if not m:
            continue
        ind = len(m.group(1))
        while stack[-1][0] >= ind:
            stack.pop()
        if m.group(4):
            stack[-1][1].append((None, m.group(4)))
        elif m.group(3):
            stack[-1][1].append((m.group(2), m.group(3)))
        else:
            children = []
            stack[-1][1].append((m.group(2), children))
            stack.append((ind, children))
    return root


@then(parsers.re(rf"the mkdocs nav has no top-level {q('title')} section"))
def _nav_lacks(world, title):
    nav = _nav_tree((REPO / "mkdocs.yml").read_text(encoding="utf-8"))
    titles = [t for t, _ in nav]
    assert title not in titles, titles


@then(
    parsers.re(
        rf"the mkdocs nav's {q('section')} section lists the groups and pages "
        rf"of {q('index')}, in order"
    )
)
def _nav_mirrors_index(world, section, index):
    nav = dict(_nav_tree((REPO / "mkdocs.yml").read_text(encoding="utf-8")))[section]
    base = index.rsplit("/", 1)[0].removeprefix("docs/")
    in_nav = [(t, [(pt, p) for pt, p in kids]) for t, kids in nav if isinstance(kids, list)]
    assert (None, f"{base}/index.md") in nav, f"{base}/index.md is not the section's index"
    in_index, group = [], None
    for ln in (REPO / index).read_text(encoding="utf-8").splitlines():
        if ln.startswith("## "):
            group = (ln[3:].strip(), [])
            in_index.append(group)
        elif group and (m := re.match(r"- \[(.+?)\]\((?!https?:)([^)#]+)\)", ln)):
            group[1].append((m.group(1), f"{base}/{m.group(2)}"))
    loose = [p for t, p in nav if not isinstance(p, list) and p != f"{base}/index.md"]
    assert not loose, f"pages outside any group in the nav: {loose}"
    assert in_nav == in_index, f"nav:   {in_nav}\nindex: {in_index}"


def _glossary_terms(doc):
    """(section, term, has explicit id) for each glossary entry: a paragraph
    that opens with a bold term (optionally after an anchor span)."""
    terms, section = [], None
    for para in re.split(r"\n\s*\n", (REPO / doc).read_text(encoding="utf-8")):
        if para.startswith("## "):
            section = para.splitlines()[0][3:]
            continue
        m = re.match(r"(?:<span[^>]*></span>)?\*\*(.+?)\*\*(\{ #[\w-]+ \})?:", para)
        if section and m:
            terms.append((section, m.group(1), bool(m.group(2))))
    assert terms, f"no glossary terms in {doc}"
    return terms


@then(parsers.re(rf"every term in {q('doc')} has an explicit anchor id"))
def _glossary_ids(world, doc):
    missing = [t for _, t, has_id in _glossary_terms(doc) if not has_id]
    assert not missing, f"terms without {{ #id }}: {missing}"


@then(parsers.re(rf"the terms in each section of {q('doc')} are in alphabetical order"))
def _glossary_sorted(world, doc):
    by_section = {}
    for section, term, _ in _glossary_terms(doc):
        by_section.setdefault(section, []).append(term)
    for section, terms in by_section.items():
        key = [t.replace("`", "").lower() for t in terms]
        assert key == sorted(key), f"{section}: {terms}"


@then(
    parsers.re(
        rf"every docs paragraph outside {q('home')} that matches {q('pattern')} "
        rf"links to {q('link')}"
    )
)
def _canonical_home(world, home, pattern, link):
    docs = REPO / "docs"
    bad = []
    for page in sorted(docs.rglob("*.md")):
        if page.relative_to(docs).as_posix() == home:
            continue
        for para in re.split(r"\n\s*\n", page.read_text(encoding="utf-8")):
            if re.search(pattern, para) and link not in para:
                bad.append(f"{page.relative_to(docs)}: {' '.join(para.split())[:120]}")
    assert (docs / home).is_file(), home
    assert re.search(pattern, (docs / home).read_text(encoding="utf-8")), f"{home} lacks it"
    assert not bad, "restated without linking the canonical page:\n" + "\n".join(bad)
