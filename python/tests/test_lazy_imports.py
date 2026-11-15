"""Worker startup leaves optional modules unimported until a run needs them.

Each spawned worker pays its imports before its first test, so the schema
TypedDicts (annotations only), junit (xml.etree), netguard (socket) and
`packaging` (the plugin pin check) load only when used. Checked in a fresh
interpreter: this test process has long since imported all of them.
"""

import os
import subprocess
import sys
from pathlib import Path

PYTHON_DIR = Path(__file__).resolve().parents[1]

LAZY = (
    "rstest_worker._internal.messages",
    "rstest_worker._internal.junit",
    "rstest_worker._internal.netguard",
    "packaging.requirements",
)


def _loaded_after(code: str) -> set[str]:
    script = (
        "import sys\n"
        "import rstest_worker.__main__\n"
        f"{code}\n"
        f"print('\\n'.join(m for m in {LAZY!r} if m in sys.modules))\n"
    )
    env: dict[str, str] = dict(os.environ)
    env["PYTHONPATH"] = str(PYTHON_DIR)
    for var in ("RSTEST_JUNITXML", "RSTEST_NETWORK", "RSTEST_WORKER_ID"):
        env.pop(var, None)
    out = subprocess.run(
        [sys.executable, "-c", script], env=env, capture_output=True, text=True, check=True
    )
    return set(out.stdout.split())


def test_worker_startup_skips_optional_modules():
    assert _loaded_after("") == set()


def test_pin_check_skips_packaging_without_a_pytest_requirement():
    code = (
        "from rstest_worker._internal.plugincompat import _pytest_pin_conflicts\n"
        "class D:\n"
        "    metadata = {'Name': 'plain'}\n"
        "    project_name = 'plain'\n"
        "    version = '1.0'\n"
        "    requires = ['attrs>=22']\n"
        "assert _pytest_pin_conflicts([(None, D())], '9.1.1') == []\n"
        "assert _pytest_pin_conflicts([], '9.1.1') == []\n"
    )
    assert _loaded_after(code) == set()
