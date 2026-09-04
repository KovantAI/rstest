"""Cross-language guard: the Python worker's protocol `kind` sets must match the
Rust orchestrator's. The Rust enums in scheduling/proto.rs are the source of
truth; python/rstest_worker/_internal/messages.py mirrors them. If either side
adds or renames a message, this test fails until the other side follows.
"""

import re
from pathlib import Path
from typing import get_args

from rstest_worker._internal import messages

PROTO_RS = Path(__file__).parents[2] / "crates" / "rstest-cli" / "src" / "scheduling" / "proto.rs"


def _snake(camel: str) -> str:
    """Rust variant ident -> serde snake_case wire discriminant."""
    return re.sub(r"(?<!^)(?=[A-Z])", "_", camel).lower()


def _rust_variants(enum_name: str) -> set[str]:
    """The snake_case `kind` strings of a proto.rs enum's variants."""
    src = PROTO_RS.read_text(encoding="utf-8")
    body = re.search(rf"enum {enum_name} \{{(.*?)\n\}}", src, re.DOTALL)
    assert body is not None, f"enum {enum_name} not found in {PROTO_RS}"
    # Variant idents sit at 4-space indent, CamelCase; fields (8-indent) and
    # attributes (#[...]) never start with an uppercase letter at that column.
    return {_snake(v) for v in re.findall(r"^    ([A-Z][A-Za-z0-9]*)", body.group(1), re.MULTILINE)}


def test_event_kinds_match_rust():
    python_kinds = set(get_args(messages.EventKind))
    assert python_kinds == _rust_variants("Event")


def test_command_kinds_match_rust():
    python_kinds = set(get_args(messages.CommandKind))
    assert python_kinds == _rust_variants("Command")


def test_kind_literals_are_nonempty():
    # Guards against get_args returning () if EventKind/CommandKind stop being
    # Literal aliases (which would make the parity asserts vacuously pass).
    assert len(get_args(messages.EventKind)) == 19
    assert len(get_args(messages.CommandKind)) == 12
