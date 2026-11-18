"""The rstest version this worker ships with: the orchestrator passes its own
in ``RSTEST_ORCHESTRATOR_VERSION`` and the worker refuses to run a different
one. Kept equal to the Cargo workspace version (a Rust test checks)."""

__version__ = "0.12.0"
