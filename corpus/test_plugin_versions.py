"""Unit tests for the corpus plugin-version probe and its docs-table renderer."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from plugin_probe import inventory, pytest_spec
from plugin_versions import aggregate, render


class _EP:
    def __init__(self, group):
        self.group = group


class _Dist:
    def __init__(self, name, version, requires=None, plugin=True):
        self.metadata = {"Name": name}
        self.version = version
        self.requires = requires
        self.entry_points = [_EP("pytest11")] if plugin else [_EP("console_scripts")]


def test_spec_plain_requirement_orders_lower_bound_first():
    assert pytest_spec(["pytest<10,>=8.4", "typing-extensions>=4"]) == (">=8.4,<10", None)


def test_spec_unversioned_is_any_and_absent_is_none():
    assert pytest_spec(["pytest"]) == ("any", None)
    assert pytest_spec(["pytest-metadata>=2"]) == (None, None)
    assert pytest_spec(None) == (None, None)


def test_spec_plain_requirement_beats_extra_gated_ones():
    reqs = ["pytest>=9 ; extra == 'test'", "pytest>=7"]
    assert pytest_spec(reqs) == (">=7", None)


def test_spec_extra_only_prefers_the_pytest_extra():
    reqs = ["pytest>=4.6 ; extra == 'all'", "pytest>=4.6 ; extra == 'pytest'"]
    assert pytest_spec(reqs) == (">=4.6", "pytest")


def test_spec_parenthesized_legacy_form():
    assert pytest_spec(["pytest (>=3.0)"]) == (">=3.0", None)


def test_inventory_keeps_plugins_and_tracked_libraries_only():
    dists = [
        _Dist("pytest_mock", "3.15.1", ["pytest>=6.2.5"]),
        _Dist("freezegun", "1.5.5", ["python-dateutil>=2.7"], plugin=False),
        _Dist("requests", "2.32", plugin=False),
        _Dist("", "0"),
    ]
    assert inventory(dists) == [
        {
            "name": "freezegun",
            "version": "1.5.5",
            "requires_pytest": None,
            "requires_pytest_extra": None,
        },
        {
            "name": "pytest-mock",
            "version": "3.15.1",
            "requires_pytest": ">=6.2.5",
            "requires_pytest_extra": None,
        },
    ]


def _plugin(name, version, spec=None, extra=None):
    return {
        "name": name,
        "version": version,
        "requires_pytest": spec,
        "requires_pytest_extra": extra,
    }


def test_render_merges_versions_and_suites_in_requested_order():
    rows = [
        {"suite": "b", "plugins": [_plugin("hypothesis", "6.167.1", ">=4.6", "pytest")]},
        {"suite": "a", "plugins": [_plugin("hypothesis", "6.9.0", ">=4.6", "pytest")]},
        {"suite": "c", "plugins": [_plugin("pytest-mock", "3.15.1", ">=6.2.5")]},
        {"suite": "d", "status": "prepare-error"},
    ]
    assert render(aggregate(rows), ["pytest-mock", "hypothesis", "pytest-html"]) == [
        "| pytest-mock | 3.15.1 | `>=6.2.5` | c |",
        "| hypothesis | 6.9.0 / 6.167.1 | `>=4.6` (`[pytest]` extra) | a, b |",
    ]


def test_render_library_without_pytest_dependency():
    rows = [{"suite": "x", "plugins": [_plugin("freezegun", "1.5.5")]}]
    assert render(aggregate(rows), ["freezegun"]) == [
        "| freezegun | 1.5.5 | none (no pytest dependency) | x |"
    ]
