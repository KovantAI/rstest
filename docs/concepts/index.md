# Concepts

How rstest works, and why it's built that way:

Evaluating rstest? Read [Compatibility](compatibility.md),
[Benchmarks](../reference/benchmarks.md), and
[Security](../reference/security.md).

- [Architecture](architecture.md): Rust orchestrator, Python workers, the vendored pytest core
- [Compatibility](compatibility.md): the contract, what's verified, known gaps
- [Scheduling](scheduling.md): item dispatch, duration cache, chunk locality, the serial phase
- [Lazy collection](lazy-collection.md): on-demand per-file collection and work-stealing
- [Crash handling](crash-handling.md): attribution, redistribution, restart budgets
- [Monorepo mode](monorepo.md): discovery, worker budget, per-flag behavior across projects
- [xdist hook emulation](xdist-hooks.md): how controller-side hooks are emulated, and where they diverge
- [Caching](caching.md): what lives in `.rstest_cache` and `.pytest_cache`, the shared remote cache backend and its trust boundary, worker temp directories
- [Glossary](glossary.md)
