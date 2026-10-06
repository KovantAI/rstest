- rstest watches the directory you started it in, recursively (not the
  project root). Start it from the root to see changes anywhere in the
  project.
- Only `.py` files and pytest configuration files trigger a rerun. Edits to
  data files, fixtures such as `.json` or `.sql`, and templates are not
  watched: save a `.py` file, or restart, to pick them up.
- A change set consisting **only of test files** (per your project's
  `python_files` patterns) reruns exactly those files, with all your other
  flags intact. Files in it that no longer exist are left out, so a change
  set of only deleted test files runs nothing.
- Any other `.py` change (source code) reruns the tests **affected by the
  change** per the project import graph (the same machinery as
  [`--changed`](changed.md)), narrowed to the affected test files; changes
  the graph can't reason about fall back to the full selection.
- Both narrowed reruns (test files, affected tests) drop the positional
  paths you started the session with and keep every flag with its value
  (`-k api`, `-n 2`, ...). Only a full-selection rerun keeps the original
  paths.
- A change that selects nothing (only deleted test files, or source that no
  test imports) skips the cycle and prints
  `[watch] change affects no tests; waiting`.
- Changes to pytest configuration files (`pytest.toml`, `.pytest.toml`,
  `pytest.ini`, `.pytest.ini`, `pyproject.toml`, `tox.ini`, `setup.cfg`)
  trigger a full rerun.
- Paths under a directory named exactly `.git`, `__pycache__`,
  `.pytest_cache`, `.rstest_cache`, `.venv`, `.gate-venv`, `node_modules`
  or `target` are ignored. Nothing else is: a virtualenv named `venv/` or
  `env/`, and `.tox/` or `.nox/`, **are** watched, so a `pip install` into
  one can trigger a rerun. Name the virtualenv `.venv`, or keep it outside
  the watched directory.
