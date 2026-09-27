# Getting started

Three commands cover the basics:

```console
$ pip install rstest
$ rstest                      # parallel run, pytest config honored
$ rstest --doctor             # and find out why the suite is slow
```

(`--doctor` shines on a real suite, not a toy two-test folder; see
[Suite diagnostics](../guides/doctor.md).)

- [Installation](installation.md): requirements, pip/uv, from source
- [Start from scratch](your-first-test.md): no suite yet? from an empty folder to a green run
- [Run your existing suite](first-steps.md): already have a pytest suite? running, reading output, selecting tests
- [Features](features.md): what rstest adds over pytest
- [Glossary](../concepts/glossary.md): worker, byte-exact mode, long pole, and the other terms
- [Troubleshooting](../reference/troubleshooting.md): first-run errors and common fixes
- [Getting help](getting-help.md)

Coming from pytest? The
[migration guide](../guides/migrate-from-pytest.md) is the page to read. Or,
in a project that already has pytest and a suite installed, run `rstest try`
for a one-command "is it worth switching?" answer (it runs your suite once
under plain pytest and once under rstest, so it takes as long as both).
