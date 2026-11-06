---
name: crew
description: Run subagents with the crew binary over claude, gemini (agy), grok, codex, and pi. Use when delegating a task, waiting on or resuming a named job, checking machine load or subscription usage, or choosing a model tier.
---

# crew

`crew` runs one agent CLI. `crew --help` is the contract.

## Run

```bash
crew <claude|gemini|grok|codex|pi|agent> [fast|medium|smart|max] [low|medium|high|max] [-m MODEL] [-t SECONDS] [-C DIR] [PROMPT | -]
```

- Every run has full autonomy. There are no permission flags. For an opinion-only run, say so in the prompt.
- A long prompt goes on stdin: `crew grok -C <dir> - < brief.md`.
- `-t` defaults to 7200 seconds. A timeout kills the run and exits 124.
- crew prints `SESSION_ID=<id>` on stderr once the id is known. Resume it with `-r <id>`. `-c` is the most recent session.
- `--add-dir` is repeatable on claude, gemini, and codex. grok and pi reject it. `--schema` forces JSON on claude, grok, and codex. `--no-web` drops web tools on claude and grok.

`agy` and `antigravity` are gemini. `gpt` and `chatgpt` are codex.

A tier (`fast`, `medium`, `smart`, `max`) picks only the model. Effort (`low`, `medium`, `high`, `max`) is the next word. Omit it and crew sends medium. A lone `medium` or `max` is the tier. `crew models` prints the table. `~/.config/crew/config.toml` (or `$XDG_CONFIG_HOME/crew/config.toml`) overrides one tier row and can remap an effort word per provider.

Exit codes: 0 ok · 75 usage, quota, rate, or credit limit · 124 timeout · 125 denied tool or incomplete run · 127 CLI missing · 128+N killed by signal N.

## Background jobs

```bash
crew job run <name> <provider> [tier] [effort] [flags] "<prompt>"
crew job wait <name>...
crew job result <name>       # stdout of the last turn
crew job log <name>          # stderr tail, then stdout
crew job peek <name>         # last 15 live steps; Gemini writes none
crew job queue <name> "<message>"   # after the current turn, same job
crew job steer <name> "<message>"   # now, same job
crew job resume <name> "<follow-up>"  # starts <name>.2 in the same session
crew job ls | show <name> | stop <name>
```

## Steer, do not stop

`crew job steer <name> "<message>"` keeps the session and the job name. Claude, pi (and a custom pi agent), and Codex keep the current step. Grok and Gemini lose that step; the command prints `[crew] current step was lost` on stderr, and the session stays. `queue` waits until the turn finishes. `result` is the last turn, after every message. `resume` starts `<name>.2` and is the wrong tool for a correction.

## Other machines

`crew @<host> <command>` runs that command over SSH. The host is `[hosts.<host>]` in config.toml (`ssh`, and optional `path` prepended on the remote). Jobs live on that machine (`crew @home job peek <name>`).

## Before spawning

- `crew load` — CPU, RAM, GPU, and how many crew agents are running. If the machine is overloaded, wait.
- `crew usage` — plan, quota bars, and reset times.

## Briefing a subagent

Tell it that it is a subagent. Give it one task, the repo or worktree and branch, what it may commit or push, the checks to run, and what to report. Parallel writers each get their own git worktree.
