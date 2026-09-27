- rstest watches the directory you started it in, recursively (not the
  project root). Start it from the root to see changes anywhere in the
  project.
- Only `.py` files and pytest configuration files trigger a rerun. Edits to
  data files, fixtures such as `.json` or `.sql`, and templates are not
  watched: save a `.py` file, or restart, to pick them up.
- A change set consisting **only of test files** (per your project's
  `python_files` patterns) reruns exactly those files, with all your other
  flags intact.
- Any other `.py` change (source code) reruns the tests **affected by the
  change** per the project import graph (the same machinery as
  [`--changed`](changed.md)); a change affecting no tests skips the rerun,
  and changes the graph can't reason about fall back to the full selection.
- Changes to pytest configuration files (`pytest.toml`, `.pytest.toml`,
  `pytest.ini`, `.pytest.ini`, `pyproject.toml`, `tox.ini`, `setup.cfg`)
  trigger a full rerun.
- Paths under a directory named exactly `.git`, `__pycache__`,
  `.pytest_cache`, `.rstest_cache`, `.venv`, `.gate-venv`, `node_modules`
  or `target` are ignored. Nothing else is: a virtualenv named `venv/` or
  `env/`, and `.tox/` or `.nox/`, **are** watched, so a `pip install` into
  one can trigger a rerun. Name the virtualenv `.venv`, or keep it outside
  the watched directory.
