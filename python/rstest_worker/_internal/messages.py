"""Typed schema for the orchestrator<->worker wire protocol.

The RUST side is the source of truth: `crates/rstest-cli/src/scheduling/proto.rs`
defines `Command`/`Event` (adjacently tagged `{"kind", "payload"}` msgpack maps,
snake_case discriminants) and the `Report`/`WarningEntry`/`FixtureStat` structs.
This module mirrors that contract as TypedDicts so `ty` checks every message the
worker builds and consumes. Keep it in sync with proto.rs — the parity test in
`python/tests/test_protocol_parity.py` fails if the two `kind` sets diverge.

TypedDicts are plain dicts at runtime: annotating a payload changes nothing about
what goes on the wire, it only lets the type checker validate field names/types.
Mixed required/optional fields use the base-class + `total=False` pattern (rather
than `NotRequired`, which is stdlib only on 3.11+; the worker targets 3.10).
"""

from __future__ import annotations

from typing import Literal, TypedDict

# ---- shared structs (mirror the Rust structs of the same name) -------------


class WarningEntry(TypedDict):
    when: str  # "config" | "collect" | "runtest"
    category: str
    message: str
    filename: str
    lineno: int
    count: int


class FixtureStat(TypedDict):
    name: str
    scope: str
    count: int
    total: float


class _ReportRequired(TypedDict):
    nodeid: str
    when: str  # "setup" | "call" | "teardown"
    outcome: str  # "passed" | "failed" | "skipped"
    duration: float
    longrepr: str | None  # present but nullable
    wasxfail: bool  # the worker always sends this


class ReportPayload(_ReportRequired, total=False):
    lineno: int  # 0-based source line
    cpu: float  # doctor mode: call-phase CPU time
    sections: list[list[str]]  # [name, content] pairs; wire arrays
    skip_reason: str


# ---- Event payloads (worker -> orchestrator) -------------------------------


class _CollectionDoneRequired(TypedDict):
    count: int
    hash: str  # sha256 of the newline-joined nodeids


class CollectionDonePayload(_CollectionDoneRequired, total=False):
    # Only worker 0 (RSTEST_SEND_IDS=1) ships the id-bearing fields.
    ids: list[str]
    locations: list[list[str | int | None]]  # [relpath, lineno] per item
    marks: list[list[str]]  # marker names per item
    cache_dir: str
    serial: list[int]  # indices of @pytest.mark.serial items
    flaky: dict[str, int]  # stringified index -> rerun budget
    groups: dict[str, str]  # stringified index -> xdist_group name


class _FileCollectedRequired(TypedDict):
    path: str
    ids: list[str]


class FileCollectedPayload(_FileCollectedRequired, total=False):
    serial: list[str]  # nodeids with the serial marker
    flaky: dict[str, int]  # nodeid -> rerun budget


class LazyReadyPayload(TypedDict, total=False):
    cache_dir: str


class DonePayload(TypedDict):
    exitstatus: int


class ItemStartPayload(TypedDict):
    index: int


class ItemDonePayload(TypedDict):
    index: int


class StoppedPayload(TypedDict):
    unrun: list[int]
    reason: str


class ItemStartIdPayload(TypedDict):
    id: str


class ItemDoneIdPayload(TypedDict):
    id: str


class StoppedIdsPayload(TypedDict):
    unrun: list[str]


class WarningsPayload(TypedDict):
    entries: list[WarningEntry]


class DoctorFixturesPayload(TypedDict):
    fixtures: list[FixtureStat]


class CollectErrorPayload(TypedDict):
    path: str
    longrepr: str


class CollectSkipPayload(TypedDict):
    path: str


class NodeInputPayload(TypedDict):
    workerinput: dict[str, object]  # _wire_safe'd, arbitrary map


EventKind = Literal[
    "report",
    "collect_error",
    "collect_skip",
    "doctor_fixtures",
    "warnings",
    "collection_done",
    "lazy_ready",
    "file_collected",
    "item_start_id",
    "item_done_id",
    "stopped_ids",
    "node_input",
    "item_start",
    "item_done",
    "stopped",
    "done",
]


# ---- Command payloads + envelopes (orchestrator -> worker) -----------------


class RunArgsPayload(TypedDict):
    args: list[str]


class RunItemsPayload(TypedDict):
    indices: list[int]


class RunFilesPayload(TypedDict):
    paths: list[str]


class RunIdsPayload(TypedDict):
    ids: list[str]


class NodeDownPayload(TypedDict):
    workerinput: dict[str, object]
    error: str


# Full inbound envelopes, for discriminated-union narrowing on `kind`.
class CmdRunTests(TypedDict):
    kind: Literal["run_tests"]
    payload: RunArgsPayload


class CmdRunItemsSession(TypedDict):
    kind: Literal["run_items_session"]
    payload: RunArgsPayload


class CmdRunLazySession(TypedDict):
    kind: Literal["run_lazy_session"]
    payload: RunArgsPayload


class CmdRunItems(TypedDict):
    kind: Literal["run_items"]
    payload: RunItemsPayload


class CmdRunFiles(TypedDict):
    kind: Literal["run_files"]
    payload: RunFilesPayload


class CmdRunIds(TypedDict):
    kind: Literal["run_ids"]
    payload: RunIdsPayload


class CmdNodeDown(TypedDict):
    kind: Literal["node_down"]
    payload: NodeDownPayload


# Unit commands carry only `kind` (Rust serializes the unit variant with no
# `payload` field).
class CmdNoMoreItems(TypedDict):
    kind: Literal["no_more_items"]


class CmdEndSession(TypedDict):
    kind: Literal["end_session"]


class CmdShutdown(TypedDict):
    kind: Literal["shutdown"]


Command = (
    CmdRunTests
    | CmdRunItemsSession
    | CmdRunLazySession
    | CmdRunItems
    | CmdRunFiles
    | CmdRunIds
    | CmdNodeDown
    | CmdNoMoreItems
    | CmdEndSession
    | CmdShutdown
)

CommandKind = Literal[
    "run_tests",
    "run_items_session",
    "run_lazy_session",
    "run_items",
    "run_files",
    "run_ids",
    "node_down",
    "no_more_items",
    "end_session",
    "shutdown",
]
