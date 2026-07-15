# Getting started

Three commands cover the basics:

```console
$ pip install rstest          # pre-release: install from a built wheel for now, see Installation
$ rstest                      # parallel run, pytest config honored
$ rstest --doctor             # and find out why the suite is slow
```

(`--doctor` shines on a real suite, not a toy two-test folder — see the
[diagnostics guide](../guides/doctor.md).)

- [Installation](installation.md) — requirements, wheels, from source
- [Your first test](your-first-test.md) — no suite yet? from an empty folder to a green run
- [First steps](first-steps.md) — already have a pytest suite? running, reading output, selecting tests
- [Features](features.md) — what rstest adds over pytest
- [Glossary](../concepts/glossary.md) — worker, byte-exact, long-pole, and the other terms
- [Troubleshooting](../reference/troubleshooting.md) — first-run errors and common fixes
- [Getting help](getting-help.md)

Coming from pytest? The
[migration guide](../guides/migrate-from-pytest.md) is the page to read — or,
in a project that already has pytest and a suite installed, run `rstest --try`
for a 30-second "is it worth switching?" answer.
