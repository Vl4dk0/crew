# Migration

This repository does not replace `claude-run`, `codex-run`, `agy-run`, `grok-run`, `baton`, `subagentcounter`, or `usagebar`. No skill, dotfile, or `PATH` entry was changed. The shims below are a proposal. Do not install them until you want the cutover.

## Commands

| Today | crew | Notes |
| --- | --- | --- |
| `claude-run` | `crew claude` | Same flags. `--tier` is new. Default timeout is 1800s, not 300s. Default effort is `medium`. |
| `agy-run` | `crew gemini` | Aliases: `antigravity`, `agy`. A timeout kills the process group. The old script only killed the child. |
| `grok-run` | `crew grok` | `--tier` selects `grok-4.7` only. `-e` selects effort. |
| `codex-run` | `crew codex` | Aliases: `gpt`, `chatgpt`. Default effort is `medium`. The wrapper's default was `low`. User-facing `max` is sent as `ultra`, except on `gpt-6-luna`. |
| `baton run` | `crew job run` | Store is `$CREW_HOME` or `~/.local/share/crew`, never `BATON_HOME`. |
| `baton ls/show/result/log/wait/resume/stop` | `crew job …` | Status is derived, not stored. `wait` returns 130 when the run was stopped. |
| `subagentcounter` | `crew load` | Same lines, plus a `crew:` line. |
| `usagebar` | `crew usage` | Claude bars come from Claude Code's `get_usage` control request, not a `/usage` screen scrape. Gemini's `/usage` runs on a PTY. |

## Proposed shims

After you are ready, a shim can be a symlink or a two-line script. Example, not installed:

```sh
#!/bin/sh
exec crew claude "$@"
```

The same shape covers `codex-run`, `agy-run` (`crew gemini`), and `grok-run`. `baton` is not a drop-in: the subcommand is `job`, the store moved, and `run` takes the provider as an argument (`crew job run <name> <provider> …`). Keep calling `baton` until a job has been recreated under crew.

`usagebar` and `subagentcounter` can become `exec crew usage` and `exec crew load`.

## What stays different on purpose

- `-m` still wins over `--tier`.
- A flag the underlying CLI does not have is exit 2. grok has no `--add-dir`. gemini has no `--schema` and no `--no-web`.
- Codex at a full weekly quota is not special-cased.
