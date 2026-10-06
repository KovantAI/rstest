!!! tip "Pin for reproducible CI"
    The recipes use a bare `pip install rstest`. For reproducible builds,
    pin an exact version (`pip install rstest==0.8.0`) or install from your
    lockfile, ideally with hashes (`pip install --require-hashes -r
    requirements.txt`). rstest is alpha (0.x), so a range like `~=0.8` can still
    pull in breaking changes.

    Pin the GitHub action and the pre-commit `rev:` the same way, to a tag or
    a full commit SHA. Use the action at `v0.8.0` or later; see
    [Security: GitHub action inputs](../reference/security.md#github-action-inputs).
