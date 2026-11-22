"""Unit tests for the module-level hash behind --incremental's import guard."""

import io
import json

from rstest_worker import modhash

BASE = '''import os

LIMIT = 3


def helper(x, default=LIMIT):
    """Doc."""
    y = x + 1
    return y


class Box:
    size = 2

    @staticmethod
    def area(n):
        return n * n


def one(): return 1
'''


def h(src, **kw):
    return modhash.module_hash(src, **kw)[0]


def test_function_body_edits_do_not_move_the_hash():
    assert h(BASE) == h(BASE.replace("y = x + 1", "y = x + 2"))
    assert h(BASE) == h(BASE.replace("return n * n", "return n ** 2"))
    # A line added to a body, not just changed.
    assert h(BASE) == h(BASE.replace("    return y\n", "    z = 0\n    return y\n"))


def test_import_time_code_moves_the_hash():
    for old, new in [
        ("LIMIT = 3", "LIMIT = 4"),  # module constant
        ("default=LIMIT", "default=5"),  # default arg: evaluated at def time
        ("def helper(x,", "def helper(xx,"),  # signature
        ("size = 2", "size = 3"),  # class body
        ("@staticmethod", "@classmethod"),  # decorator
        ("import os", "import sys"),
    ]:
        assert h(BASE) != h(BASE.replace(old, new)), old


def test_one_line_def_is_kept_whole():
    assert h(BASE) != h(BASE.replace("def one(): return 1", "def one(): return 2"))


def test_docstring_only_body_is_kept_whole():
    # Calling it records only its header line, never a body line, so no test
    # would look like it ran it: an edit to it must move the module hash.
    src = 'def stub():\n    """Doc."""\n'
    _, kept, defs = modhash.analyze(src)
    assert kept == ["stub"] and defs == {}
    assert h(src) != h(src + "    raise ValueError\n")


def test_bodies_that_ran_outside_a_call_phase_are_kept():
    # Line 8 (`y = x + 1`) ran at import or in a fixture: helper's body counts.
    digest, kept = modhash.module_hash(BASE, lines={8})
    assert kept == ["helper", "one"]
    edited = BASE.replace("y = x + 1", "y = x + 2")
    assert modhash.module_hash(edited, lines={8})[0] != digest
    # A later check names the function instead, so moved lines don't matter.
    moved = "# a new first line\n" + edited
    assert modhash.module_hash(moved, keep={"helper"})[0] != digest
    # A comment line is not code: it moves lines, not the hash.
    assert modhash.module_hash("# a new first line\n" + BASE, keep={"helper"})[0] == digest
    assert modhash.module_hash(BASE, keep={"helper"})[0] == digest
    # Methods are named through their class.
    assert "Box.area" in modhash.module_hash(BASE, lines={17})[1]


def test_same_named_defs_are_told_apart():
    # Property getter and setter share a qualified name; only the getter ran
    # outside a call phase. Checking by the recorded names must reproduce the
    # recorded hash, so the setter must not be kept by its namesake.
    src = (
        "class C:\n"
        "    @property\n"
        "    def value(self):\n"
        "        return 1\n"
        "\n"
        "    @value.setter\n"
        "    def value(self, v):\n"
        "        self.v = v\n"
    )
    digest, kept = modhash.module_hash(src, lines={4})
    assert modhash.module_hash(src, keep=set(kept))[0] == digest
    # The setter body is still left out: editing it doesn't move the hash.
    edited = src.replace("self.v = v", "self.v = v + 1")
    assert modhash.module_hash(edited, keep=set(kept))[0] == digest


def test_defs_under_compound_statements_are_found():
    src = (
        "import sys\n"
        "if sys.version_info > (3,):\n"
        "    def f():\n"
        "        return 1\n"
        "else:\n"
        "    def f():\n"
        "        return 2\n"
    )
    assert h(src) == h(src.replace("return 1", "return 3").replace("return 2", "return 4"))


def test_main_answers_per_file_and_leaves_out_unparseable(tmp_path, monkeypatch):
    (tmp_path / "a.py").write_text(BASE)
    (tmp_path / "bad.py").write_text("def broken(:\n")
    request = {
        "root": str(tmp_path),
        "files": {"a.py": {"lines": [8]}, "bad.py": {}, "missing.py": {"keep": []}},
    }
    monkeypatch.setattr("sys.stdin", io.StringIO(json.dumps(request)))
    out = io.StringIO()
    monkeypatch.setattr("sys.stdout", out)
    modhash.main()
    answer = json.loads(out.getvalue())
    assert list(answer) == ["a.py"]
    digest, kept, defs = modhash.analyze(BASE, lines={8})
    assert answer["a.py"] == {
        "hash": digest,
        "keep": kept,
        "defs": {k: list(v) for k, v in defs.items()},
    }


def test_analyze_hashes_each_left_out_body_on_its_own():
    _, kept, defs = modhash.analyze(BASE)
    assert kept == ["one"]
    assert sorted(defs) == ["Box.area", "helper"]
    # Body spans start after the header (docstring included) and end the def.
    assert defs["helper"][:2] == (7, 9)
    assert defs["Box.area"][:2] == (17, 17)
    # Editing one body moves only that body's hash, not the other's.
    _, _, edited = modhash.analyze(BASE.replace("return n * n", "return n ** 2"))
    assert edited["Box.area"][2] != defs["Box.area"][2]
    assert edited["helper"] == defs["helper"]
    # A body kept in the module-level hash is not listed on its own.
    assert "helper" not in modhash.analyze(BASE, keep={"helper"})[2]


def test_comments_and_blank_lines_move_no_hash():
    digest, _, defs = modhash.analyze(BASE)
    for edited in [
        BASE.replace("    y = x + 1", "    # bump it\n    y = x + 1  # by one"),
        BASE.replace("LIMIT = 3", "LIMIT = 3  # tuned"),
        BASE.replace("import os\n", "import os\n\n\n# helpers below\n"),
    ]:
        d2, _, defs2 = modhash.analyze(edited)
        assert d2 == digest, edited
        assert {k: v[2] for k, v in defs2.items()} == {k: v[2] for k, v in defs.items()}


def test_strings_docstrings_and_encoding_still_count():
    digest, _, defs = modhash.analyze(BASE)
    # A `#` inside a string is code.
    assert modhash.analyze(BASE.replace("import os", "import os\nTAG = 'a#b'"))[0] != digest
    tagged = modhash.analyze(BASE.replace("import os", "import os\nTAG = 'a#c'"))[0]
    assert tagged != modhash.analyze(BASE.replace("import os", "import os\nTAG = 'a#b'"))[0]
    # Docstrings stay: doctests and __doc__ read them.
    _, _, edited = modhash.analyze(BASE.replace('"""Doc."""', '"""Other doc."""'))
    assert edited["helper"][2] != defs["helper"][2]
    # An encoding declaration changes how the file decodes.
    latin = "# -*- coding: latin-1 -*-\n" + BASE
    utf8 = "# -*- coding: utf-8 -*-\n" + BASE
    assert modhash.analyze(latin)[0] != modhash.analyze(utf8)[0]


def test_layout_follows_a_moved_body_but_not_one_reshaped_inside():
    src = "def a():\n    return 1\n\n\ndef b():\n    x = 1\n    return x\n"
    _, _, defs = modhash.analyze(src)
    before = modhash.layout(src, defs)
    moved = "\n\n" + src.replace("return 1", "return 2")
    _, _, mdefs = modhash.analyze(moved)
    assert modhash.layout(moved, mdefs)["b"] == before["b"]
    assert mdefs["b"][0] == defs["b"][0] + 2
    reshaped = src.replace("    x = 1\n", "    x = 1\n    # note\n")
    _, _, rdefs = modhash.analyze(reshaped)
    assert rdefs["b"][2] == defs["b"][2], "comment-insensitive body hash"
    assert modhash.layout(reshaped, rdefs)["b"] != before["b"]
