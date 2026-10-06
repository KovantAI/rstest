# Persist the duration cache: from the second run on, the scheduler
# starts the slowest tests first.
- uses: actions/cache@v6
  with:
    # Replay journals are per-run; upload them on failure instead
    # (see the Replaying a CI failure locally guide).
    path: |
      .rstest_cache
      !.rstest_cache/replay
    # Same components as the bundled action: OS + Python + lockfile
    # hash, so a 3.12 or Windows run never seeds a 3.13 Linux one.
    # Unique per run: actions/cache never RE-saves an existing key,
    # so a fixed key freezes the cache at its first run. restore-keys
    # picks the newest match (this branch first, then the base branch).
    key: rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-${{ github.run_id }}
    restore-keys: |
      rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-
