!!! tip "Pin for reproducible CI"
    The recipes use a bare `pip install rstest`. For reproducible builds,
    pin an exact version (`pip install rstest==0.7.0`) or install from your
    lockfile, ideally with hashes (`pip install --require-hashes -r
    requirements.txt`). rstest is pre-1.0, so a range like `~=0.7` can still
    pull in breaking changes.

    Pin the GitHub action and the pre-commit `rev:` the same way, to a tag or
    a full commit SHA. For 0.7.0, pin the action to a `main` commit SHA rather
    than the `v0.7.0` tag; see
    [Security: GitHub action inputs](../reference/security.md#github-action-inputs).
