# Concepts

How rstest works, and why it's built that way:

- [Architecture](architecture.md) — Rust orchestrator, Python workers, the vendored pytest core
- [Compatibility](compatibility.md) — the contract, what's verified, known gaps
- [Scheduling](scheduling.md) — item dispatch, duration cache, chunk locality, the serial phase
- [Lazy collection](lazy-collection.md) — on-demand per-file collection and work-stealing
- [Crash handling](crash-handling.md) — attribution, redistribution, restart budgets
- [Monorepo mode](monorepo.md) — discovery, worker budget, per-flag behavior across projects
- [xdist hook emulation](xdist-hooks.md) — how master-side hooks are emulated, and where they diverge
- [Caching](caching.md) — what lives in `.rstest_cache` and `.pytest_cache`
- [Glossary](glossary.md)
