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
- VCS internals, `__pycache__`, virtualenvs, and rstest's own caches are
  ignored.
