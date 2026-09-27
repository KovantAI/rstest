!!! tip "Pin for reproducible CI"
    The recipes use a bare `pip install rstest`. For reproducible builds,
    pin an exact version (`pip install rstest==0.7.0`) or install from your
    lockfile, ideally with hashes (`pip install --require-hashes -r
    requirements.txt`). rstest is pre-1.0, so a range like `~=0.7` can still
    pull in breaking changes.
