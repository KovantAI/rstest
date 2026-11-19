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


def test_bodies_that_ran_outside_a_call_phase_are_kept():
    # Line 8 (`y = x + 1`) ran at import or in a fixture: helper's body counts.
    digest, kept = modhash.module_hash(BASE, lines={8})
    assert kept == ["helper", "one"]
    edited = BASE.replace("y = x + 1", "y = x + 2")
    assert modhash.module_hash(edited, lines={8})[0] != digest
    # A later check names the function instead, so moved lines don't matter.
    moved = "# a new first line\n" + edited
    assert modhash.module_hash(moved, keep={"helper"})[0] != digest
    assert modhash.module_hash("# a new first line\n" + BASE, keep={"helper"})[0] != digest
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
    assert answer["a.py"] == {
        "hash": modhash.module_hash(BASE, lines={8})[0],
        "keep": ["helper", "one"],
    }
