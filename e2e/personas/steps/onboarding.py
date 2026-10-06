"""Steps for features/onboarding.feature: the first-time evaluator."""

import json
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


@then(parsers.re(rf"no paragraph of {q('glob')} matches {q('pattern')}"))
def _no_paragraph_matches(world, glob, pattern):
    pages = sorted(REPO.glob(glob))
    assert pages, f"{glob} matches no file"
    bad = [
        f"{page.relative_to(REPO)}: {' '.join(para.split())[:160]}"
        for page in pages
        for para in re.split(r"\n\s*\n", page.read_text(encoding="utf-8"))
        if re.search(pattern, " ".join(para.split()))
    ]
    assert not bad, f"matches {pattern!r}:\n" + "\n".join(bad)


@then(parsers.re(rf"that level-2 section names the link {q('target')}"))
def _section_links(world, target):
    assert f"]({target})" in world.notes["section"], world.notes["section"][:200]


@then(parsers.re(rf"that level-2 section names each of {q('flags')}"))
def _section_names(world, flags):
    sec = world.notes["section"]
    missing = [f for f in flags.split() if not re.search(rf"`[^`]*{re.escape(f)}\b", sec)]
    assert not missing, f"not named in the section: {missing}"


def _line_count(ranges):
    """How many lines `7, 12-14` names (4)."""
    n = 0
    for part in ranges.split(","):
        lo, _, hi = part.strip().partition("-")
        n += int(hi or lo) - int(lo) + 1
    return n


@then("every diff-coverage report in the docs counts as many uncovered lines as it lists")
def _diff_cov_examples(world):
    seen = 0
    for page in sorted((REPO / "docs").rglob("*.md")):
        text = page.read_text(encoding="utf-8")
        for m in re.finditer(r"\((\d+)/(\d+) added lines covered\)\n((?:  .+\n)+)", text):
            seen += 1
            covered, total = int(m.group(1)), int(m.group(2))
            lists = re.findall(r"uncovered added line\(s\) ([\d, -]+)", m.group(3))
            listed = sum(_line_count(r) for r in lists)
            assert total - covered == listed, (
                f"{page.relative_to(REPO)}: {covered}/{total} covered leaves "
                f"{total - covered} uncovered, but the report lists {listed}"
            )
    assert seen, "no diff-coverage report in the docs"


@then(
    parsers.re(
        rf"every docs paragraph on {q('suite')} parity states the score and mismatch count "
        rf"in {q('results')}"
    )
)
def _parity_figures(world, suite, results):
    row = next(r for r in json.loads((REPO / results).read_text()) if r["suite"] == suite)
    seen = 0
    for page in sorted((REPO / "docs").rglob("*.md")):
        for para in re.split(r"\n\s*\n", page.read_text(encoding="utf-8")):
            para = " ".join(para.split())
            pcts = re.findall(r"(\d+\.\d+)%", para)
            if not (re.search(suite, para, re.I) and pcts):
                continue
            seen += 1
            where = f"{page.relative_to(REPO)}: {para[:120]}"
            assert all(float(p) == row["score"] for p in pcts), f"{pcts} vs {row['score']}: {where}"
            for n in re.findall(r"(\d+) IMV", para):
                assert int(n) == row["mismatch_count"], f"{n} vs {row['mismatch_count']}: {where}"
    assert seen, f"no docs paragraph states {suite} parity"


SITE = "https://python-rstest.readthedocs.io/en/stable/"


def _site_links(md):
    """Relative `.md` links rewritten to the published site's URLs, as the
    README (rendered on GitHub and PyPI) has to spell them."""

    def url(m):
        path, _, frag = m.group(2).partition("#")
        page = re.sub(r"(/?index)?\.md$", "/", path)
        return f"[{m.group(1)}]({SITE}{page}{'#' + frag if frag else ''})"

    return re.sub(r"\[([^\]]+)\]\((?!https?:)([^)]+\.md(?:#[^)]*)?)\)", url, md)


@then(
    parsers.re(
        rf"the README list under the {q('marker')} marker is the {q('heading')} list "
        rf"of {q('doc')}, with site links"
    )
)
def _readme_mirrors(world, marker, heading, doc):
    readme = (REPO / "README.md").read_text(encoding="utf-8")
    tag = f"<!-- SOURCE OF TRUTH: {marker}"
    assert tag in readme, f"README has no {tag!r} marker"
    got = readme.split(tag, 1)[1].split("-->\n", 1)[1].split("\n\n", 1)[0]
    text = (REPO / doc).read_text(encoding="utf-8")
    assert f"\n{heading}\n" in text, f"{doc} has no {heading!r}"
    want = text.split(f"\n{heading}\n", 1)[1].strip().split("\n\n", 1)[0]
    assert got.strip() == _site_links(want), f"README drifted from {doc} {heading}"


def _verdict_rows(page):
    """{plugin: [cells]} for each table of `page` whose header has a verdict
    column (`Tier`, `Status (verdict)` or `Verdict`)."""
    rows, in_table = {}, False
    for ln in page.read_text(encoding="utf-8").splitlines():
        if not ln.startswith("|"):
            in_table = False
            continue
        cells = [c.strip() for c in ln.strip("|").split("|")]
        if not in_table:
            in_table = any(re.match(r"(Tier|Status|Verdict)\b", c) for c in cells)
            continue
        if in_table and not set(cells[0]) <= set("-: "):
            rows.setdefault(cells[0], []).append(cells)
    return rows


@then(parsers.re(rf"no plugin has a verdict row on more than one page of {q('glob')}"))
def _one_verdict_row(world, glob):
    seen = {}
    for page in sorted(REPO.glob(glob)):
        for plugin in _verdict_rows(page):
            seen.setdefault(plugin, []).append(page.name)
    dup = {p: pages for p, pages in seen.items() if len(pages) > 1}
    assert seen, f"no verdict table in {glob}"
    assert not dup, f"verdict rows repeated across pages: {dup}"


@then(parsers.re(rf"every plugin in {q('doc')} has the verdict and V/i mark of {q('matrix')}"))
def _verdicts_agree(world, doc, matrix):
    mine = _verdict_rows(REPO / doc)
    # The matrix's columns: rank | plugin | downloads | verdict | V/i | note.
    ref = {r[1]: (r[3], r[4]) for rows in _verdict_rows(REPO / matrix).values() for r in rows}
    checked, bad = 0, []
    for plugin, rows in mine.items():
        if plugin in ref:
            checked += 1
            got = (rows[0][1], rows[0][2])
            if got != ref[plugin]:
                bad.append(f"{plugin}: {got} vs {ref[plugin]}")
    assert checked, f"no plugin of {doc} is in {matrix}"
    assert not bad, "\n".join(bad)


@then(
    parsers.re(
        rf"the {q('index')} entry for {q('page')} names every level-2 heading of it "
        rf"except {q('skip')}"
    )
)
def _index_names_sections(world, index, page, skip):
    base = (REPO / index).parent
    entry = next(
        ln for ln in (REPO / index).read_text(encoding="utf-8").splitlines() if f"]({page})" in ln
    )
    heads = re.findall(r"^## (.+)$", (base / page).read_text(encoding="utf-8"), re.M)
    missing = [h for h in heads if h not in skip.split(", ") and h.lower() not in entry.lower()]
    assert heads, f"{page} has no level-2 headings"
    assert not missing, f"{index} entry for {page} leaves out: {missing}"


@then(parsers.re(rf"the mkdocs nav lists {q('title')} at {q('path')}"))
def _nav_lists(world, title, path):
    def walk(nodes):
        for t, p in nodes:
            if isinstance(p, list):
                yield from walk(p)
            else:
                yield t, p

    nav = list(walk(_nav_tree((REPO / "mkdocs.yml").read_text(encoding="utf-8"))))
    assert (title, path) in nav, [n for n in nav if n[0] == title or n[1] == path]


@then(parsers.re(rf"mkdocs.yml redirects {q('old')} to {q('new')}"))
def _redirects(world, old, new):
    text = (REPO / "mkdocs.yml").read_text(encoding="utf-8")
    assert re.search(rf"^\s+{re.escape(old)}:\s*{re.escape(new)}\s*$", text, re.M), old
    assert not (REPO / "docs" / old).exists(), f"docs/{old} still exists"


def _secs(text):
    """`1m15s` / `21.0s` (the try report's fmt_secs) as seconds."""
    m = re.fullmatch(r"(?:(\d+)m)?(\d+(?:\.\d+)?)s", text)
    assert m, text
    return int(m.group(1) or 0) * 60 + float(m.group(2))


@then(
    parsers.re(rf"every `rstest try` saves line in {q('glob')} has the report's 30-day projection")
)
def _try_saves(world, glob):
    lines = [
        (p, ln)
        for p in sorted(REPO.glob(glob))
        for ln in p.read_text(encoding="utf-8").splitlines()
        if "💸 saves" in ln
    ]
    assert lines, f"no try report in {glob}"
    for page, ln in lines:
        m = re.search(
            r"saves   (\S+) per run — ≈ (\S+) over your last 30 days \((\d+) commits ≈ CI runs\)",
            ln,
        )
        assert m, f"{page.relative_to(REPO)}: {ln.strip()}"
        assert abs(_secs(m.group(1)) * int(m.group(3)) - _secs(m.group(2))) < 1, ln


def _level2_section(doc, heading):
    """The body of `heading` (a level-2 heading line) in `doc`, up to the next
    level-2 heading. Fails when the heading is missing, rather than silently
    searching the whole page."""
    text = (REPO / doc).read_text(encoding="utf-8")
    assert f"\n{heading}\n" in text, f"{doc} has no {heading!r} heading"
    return text.split(f"\n{heading}\n", 1)[1].split("\n## ", 1)[0]


@then(
    parsers.re(
        rf"the {q('heading')} section of {q('doc')} quotes the django-allauth `-n 4` time "
        rf"from {q('bench')}"
    )
)
def _allauth_time(world, heading, doc, bench):
    row = next(
        ln
        for ln in (REPO / bench).read_text(encoding="utf-8").splitlines()
        if ln.startswith("| django-allauth |")
    )
    want = re.search(r"\(([\d.]+s) at its recommended `-n 4`\)", row)
    assert want, row
    sec = _level2_section(doc, heading)
    said = re.findall(r"([\d.]+s) at\s+`-n 4`", " ".join(sec.split()))
    assert said == [want.group(1)], f"{doc} {heading} says {said}, {bench} says {want.group(1)}"


@then(parsers.re(rf"the first command in {q('doc')} is {q('command')}"))
def _first_command(world, doc, command):
    text = (REPO / doc).read_text(encoding="utf-8")
    first = re.search(r"^\$ (.*)$", text, re.M)
    assert first, f"{doc} shows no command"
    assert first.group(1).split("#")[0].strip() == command, first.group(0)


@then(parsers.re(rf"{q('doc')} ends with a {q('heading')} section linking each of {q('links')}"))
def _ends_with_links(world, doc, heading, links):
    text = (REPO / doc).read_text(encoding="utf-8")
    last = text.rsplit("\n## ", 1)[-1]
    assert f"## {last}".startswith(heading + "\n"), f"{doc} ends with ## {last.splitlines()[0]}"
    missing = [t for t in links.split() if f"]({t})" not in last]
    assert not missing, f"{heading} lacks links to {missing}"


@then(parsers.re(rf"the {q('heading')} section of {q('doc')} contains {q('text')}"))
def _level2_contains(world, heading, doc, text):
    sec = _level2_section(doc, heading)
    assert text in sec, f"{heading} of {doc} lacks {text!r}"


@then(
    parsers.re(
        rf"every environment variable the CLI reads is named in {q('doc')}, "
        rf"except {q('skip')}"
    )
)
def _env_documented(world, doc, skip):
    src = REPO / "crates" / "rstest-cli" / "src"
    names = set()
    for f in src.rglob("*.rs"):
        text = f.read_text(encoding="utf-8")
        names |= set(re.findall(r'var(?:_os)?\("([A-Z][A-Z0-9_]+)"\)', text))
        # Lists looped over and read one by one (`for var in ["A", "B"]`).
        looped = r"for \w+ in \[([^\]]+)\]\s*\{\s*if let Some\([^)]*\) = std::env::var"
        for lst in re.findall(looped, text):
            names |= set(re.findall(r'"([A-Z][A-Z0-9_]+)"', lst))
    assert names, "found no env reads"
    page = (REPO / doc).read_text(encoding="utf-8")
    missing = sorted(n for n in names - set(skip.split()) if f"`{n}`" not in page)
    assert not missing, f"read by the CLI but not in {doc}: {missing}"


@then(parsers.re(rf"no table cell in {q('doc')} is longer than (?P<n>\d+) characters"))
def _short_cells(world, doc, n):
    long = [
        c.strip()[:60] + "..."
        for ln in (REPO / doc).read_text(encoding="utf-8").splitlines()
        if ln.startswith("|")
        for c in ln.strip("|").split("|")
        if len(c.strip()) > int(n)
    ]
    assert not long, f"cells over {n} chars: {long}"
