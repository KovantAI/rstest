# Agent skills

rstest ships two [Agent Skills](https://agentskills.io) that let a coding agent
(Claude Code, Codex, and others) drive rstest's multi-step workflows for you:

- **`migrate-to-rstest`**: switch a suite or its CI from pytest to rstest, make
  it parallel-safe, and speed it up. Its readiness lane drives `migrate-check`,
  applies the fix for each verdict, and sets up `[tool.rstest]` config and a CI
  gate. Its speed lane drives `--doctor` and turns each finding into an action.
- **`rstest-triage`**: debug failures on a suite that already runs on rstest: a
  CI run that is red but passes locally, a test that fails only in parallel, a
  polluter, a flaky test, a hung worker. It drives `replay`, `bisect`, `audit`,
  `explain`, and the flaky-test controls.

Both ask before they edit your tests or CI.

A skill only loads from a place the agent looks, so installing the `rstest`
package doesn't give you the skills by itself. Pick one of the two ways below.

## Option 1: from the rstest binary (any agent)

```bash
rstest install-skills
```

This writes the skills bundled with your installed rstest into
`.claude/skills/` in the current directory. Commit that directory and everyone
on the team gets the skills. The skills always match the binary they came
from, so the flags and subcommands they tell the agent to run exist in your
version. Run the command again after upgrading rstest to refresh them.

| Flag | Writes to |
|------|-----------|
| (none) | `./.claude/skills/` (Claude Code, this project) |
| `--user` | `~/.claude/skills/` (Claude Code, every project) |
| `--agents` | `./.agents/skills/` (Codex and other Agent Skills readers) |
| `--user --agents` | `~/.agents/skills/` |
| `--dir DIR` | `DIR/<skill>/` |

If a skill is already installed and its files differ from the bundled copy
(you edited it, or it came from another rstest version), it is left alone and
the command exits `1`. Add `--force` to overwrite it. `--force` replaces only
the files rstest ships and keeps any files you added to the skill directory.

```console
$ rstest install-skills
  migrate-to-rstest: installed
  rstest-triage: installed
rstest 0.8.0 skills in /path/to/project/.claude/skills. Claude Code picks up project and user skills live; if they don't show up, start a new session.
```

## Option 2: the Claude Code plugin

The rstest repository is also a Claude Code plugin marketplace. Inside Claude
Code:

```text
/plugin marketplace add KovantAI/rstest
/plugin install rstest@rstest
```

Or from a shell:

```bash
claude plugin marketplace add KovantAI/rstest
claude plugin install rstest@rstest
```

The plugin makes the skills available in every project and updates when the
rstest repository changes, so it tracks the latest rstest rather than the
version you have installed. If you pin an older rstest, use
`rstest install-skills` instead.

To suggest the plugin to everyone who opens your repository in Claude Code,
add this to the project's `.claude/settings.json`:

```json
{
  "extraKnownMarketplaces": {
    "rstest": {
      "source": { "source": "github", "repo": "KovantAI/rstest" }
    }
  },
  "enabledPlugins": {
    "rstest@rstest": true
  }
}
```

## Using the skills

The agent picks a skill when your request matches it: "migrate my suite to
rstest", "why does this test only fail on CI", "which tests are slowest". To
run one explicitly, type its slash command:

| Installed with | Commands |
|----------------|----------|
| `rstest install-skills` | `/migrate-to-rstest`, `/rstest-triage` |
| the plugin | `/rstest:migrate-to-rstest`, `/rstest:rstest-triage` |

## If the skills don't show up

- **Check that they're loaded.** In Claude Code, `/skills` lists every skill it
  found. Plugin skills appear under the `rstest` plugin.
- **Reload after a plugin install.** If `/plugin install` says to run
  `/reload-plugins`, do that, or start a new session.
- **Install where the agent looks.** `rstest install-skills` writes to the
  current directory. Run it from the project root, the directory you start the
  agent in.
- **Match the agent to the directory.** Claude Code reads `.claude/skills/`.
  Codex and other Agent Skills readers use `.agents/skills/`, so install with
  `--agents` for them.
