# crew — plan

Acked at `dbdee46`, then amended after Vladko's note in the Roof tool thread. The amendment wins where it disagrees with the sections below.

Vladko, 2026-10-20:

- `-e` accepts only `low`, `medium`, `high`, `max`. Default is `medium` when neither `--tier` nor `-e` is passed, including codex (its wrapper default is `low`). `--tier` without `-e` uses the tier row's effort.
- `max` is mapped per CLI: claude sends `max`; codex sends `ultra` except `gpt-6-luna`, which sends `max`; gemini sends `high`; grok sends `xhigh` (and `high` for `grok-4.5`, which has no `xhigh`). The user never types `xhigh` or `ultra`.
- Grok's four tiers are all `grok-4.7` and do not select effort. `-e` does, default `medium`. A grok row that sets `effort` is a config error. `grok-4.7-build-fast` stays available only through `-m`.
- `-m` overrides the tier's model. The built-in table is the default. `~/.config/crew/tiers.toml` replaces it.
- Default autonomy stays allow-all. `--auto` is the explicit spelling.
- `-t` defaults to 1800 seconds. The wrapper default of 300 was shorter than real runs.
- A finished run longer than 20 minutes prints one stderr line telling the orchestrator to rate it later with `crew review write`. crew does not file the review. `review::long_run_note` is that line. Jobs print it from `wait` and `result`.
- Flag names are shared where the behavior exists on every provider (`--tier`, `-m`, `-e`, `-t`, `-C`, `-r`, `-c`, `--print-id`, the mode flags). `--add-dir`, `--schema`, and `--no-web` are not offered on a provider whose CLI has no such switch. A fake shared flag that then no-ops is worse than exit 2.

The wrapper behavior below is taken from the four `*-run` scripts (survey `roof-read-four`). Where a skill sentence disagrees with the script, the script wins. Where this amendment disagrees with the script's defaults (timeout, effort vocabulary, codex default effort), the amendment wins.

## Name

The command is `crew`.

- `crew` — chosen. `crew claude`, `crew job ls`, `crew load` is plain English. Not on `PATH`. No Homebrew formula.
- `roof` — the working title. Also clear of `PATH` and Homebrew. Rejected as a metaphor, which is the same failure as `baton`.
- `span` — one binary spanning the jobs. Rejected as less obvious.

`desk`, `aide`, and `nest` are Homebrew formulas. `boss` is a cask. They were not candidates.

Repo: `https://github.com/Vl4dk0/crew` (private), checkout `/Users/vladko/Projects/crew`.

## CLI

No `run` subcommand. The provider is the subcommand. Vladko decided this.

```text
crew claude  [flags] [prompt|-]
crew gemini  [flags] [prompt|-]     aliases: antigravity, agy
crew grok    [flags] [prompt|-]
crew codex   [flags] [prompt|-]     aliases: gpt, chatgpt

crew job run|ls|show|result|log|wait|resume|stop …
crew load
crew usage
crew review write|query|pick …
crew models
```

`job`, `load`, `usage`, `review`, `models`, and `help` are reserved. Provider names and aliases are not reused.

`crew models` prints provider, tier, model, effort, and whether the row is the built-in default or the user file.

### Shared flags

Same spelling and default as the wrappers. `--auto` is accepted and ignored: omitting the mode flags is full autonomy.

- `--read-only`, `--write`, `--auto` — mutex
- `--tier fast|balanced|smart|max` — new. `-m` still wins for the model. Passing neither `--tier` nor `-m` keeps that wrapper's own default. It does not silently apply `balanced`.
- `-m/--model`, `-e/--effort`
- `-t/--timeout` seconds, default 1800, must be finite and `> 0`, else exit 2
- `-C/--cwd`
- `-r/--resume`, `-c/--continue`, `--print-id`
- the prompt is trailing, or `-` to read stdin. A non-`-` prompt does not touch stdin. The prompt is a child argv element, never the child's stdin. `-` on a tty is exit 2 (`pass a prompt or pipe one using -`). Empty stdin is exit 2.

`--print-id` writes `SESSION_ID=<id>` to stderr, never stdout.

A flag that one wrapper does not have is exit 2 on that provider, not a silent drop.

### Flags that only some providers have

| Flag | claude | gemini | grok | codex |
|---|---|---|---|---|
| `--add-dir` (repeatable) | raw string | raw string | no | `Path.resolve()`, no tilde expansion |
| `--schema` | `--json-schema` literal. A value that does not start with `{` is a path, tilde-expanded, file bytes become the argument | no | same as claude | `--output-schema` absolute path. A `{...}` literal is still a path. No tilde expansion |
| `--raw` | print the JSON envelope, still apply the success check and `--print-id` | no | print grok's stdout, skip the stop check and `--print-id` | print the JSONL, still apply the success check and `--print-id` |
| `--no-web` | drop `Web*` tools, or `--disallowed-tools WebSearch,WebFetch` outside read-only | no | `--disable-web-search`, and drop `web_*` from the fallback tool list | no |
| `-n/--max-turns` | no | no | `--max-turns`, omitted when 0 | no |
| `--agent` | no | `--agent`. With no `-m`, `--model` is omitted | no | no |
| `--sandbox` | no | `--sandbox`, combinable with any mode | no | no |
| `--review`, `--uncommitted`, `--base`, `--commit` | no | no | no | codex `review` subcommand. Mutex with `-r` and `-c`. A scope and a prompt together is exit 2. A scope without a prompt does not read stdin |

### Job

`crew job run` takes a provider name. The supervisor executes `crew <provider> …`, so a background job uses the same runner as a foreground one.

```text
crew job run [--note TEXT] <name> <provider> [wrapper flags] [-- <prompt>]
crew job ls [--json]
crew job show <name> [--json]
crew job result <name>          # exit 2 while running, 3 if lost
crew job log <name> [-n N]      # stderr, default 20
crew job wait <name>... [--any] [--timeout SEC]
crew job resume <name> [-- <prompt>]
crew job stop <name>
```

`wait` exits with the wrapper code, except a stopped job returns 130. `wait --timeout` expiry is exit 4. Failure to launch the provider is 127.

### Load and usage

`crew load` prints the same lines as `subagentcounter`, then a `crew:` line in the same shape (basename `crew`, skip `bash|sh|zsh|dash|ksh` whose first arg is `-c`, skip this process's own tree). The four `*-run` lines stay.

`crew usage` reproduces usagebar: mark, window label, percent, 20-cell bar. Order is the least-used window, not a fixed provider list. Marks are grok `✕`, claude `✳`, gemini `✦`, codex `⚛`. Those marks are not errors. A collect failure is text under that block. Green below 50, orange from 50, red from 80. Partial blocks `▏▎▍▌▋▊▉█`.

### Review

```text
crew review write <model> <task> <what> <duration> <cost> <score> <reviewer>
crew review query <harness|all>
crew review pick <task description>
```

`<model>` is `<model-id>-<effort>`. Duration accepts seconds or `1h30m15s`. Cost accepts a number, `nan`, or `-`. Score is an integer 0 through 100. `query` prints today's column table. `pick` prints one model id. Empty history is exit 2.

## Module layout

One binary crate. No workspace. No async runtime.

```text
src/main.rs
src/providers/mod.rs       shared flags, exit helper
src/providers/claude.rs
src/providers/gemini.rs
src/providers/grok.rs
src/providers/codex.rs
src/providers/pty.rs
src/tier.rs
src/job/{mod,store,supervisor,status}.rs
src/load.rs
src/usage/{mod,claude,codex,grok,gemini,render}.rs
src/review/{mod,store,picker}.rs
tiers.toml                 built-in default
```

`crew job run` re-execs a hidden `crew --supervise <name>`. It is not in `--help`.

Dependencies: `clap`, `serde`, `serde_json`, `toml`, and `libc` or `rustix` for the PTY and process groups. One error enum per module.

After this plan is acked:

- grok owns `src/main.rs`, `src/providers/**`, `src/tier.rs`, `src/review/**`, `tiers.toml`, README, MIGRATION.md. First commit on the implementation branch is the clap surface plus the function signatures gemini calls.
- gemini owns `src/job/**`, `src/load.rs`, `src/usage/**`, branched from that signature commit.

Each reviews the other's PR before it merges.

## Tiers

Built-in `tiers.toml` via `include_str`. Override `~/.config/crew/tiers.toml`, or `CREW_TIERS` if that path is set. Unknown keys are errors (`deny_unknown_fields`). `-m` replaces the tier's model and still uses the tier's effort when `-e` is absent.

The ids are the strings each wrapper actually puts on the CLI. Claude's wrapper does not expand aliases, and its skill's resolved haiku id (`claude-haiku-4-5-20251001`) disagrees with `reviewsubagent`'s `claude-haiku-5-5`. crew passes the short alias (`haiku`, `sonnet`, `opus`, `fable`) and lets the installed `claude` resolve it. A full id in the user file is passed through unchanged.

Codex has three families and four tiers, so `smart` and `max` share `gpt-6-astra` and differ by effort. That follows Vladko's "luna / sol / astra" list. `balanced` is the wrapper default, `gpt-6.1-sol` at `low`.

Gemini effort is baked into the slug for `flash` and `argon`. The runner must not also pass `--effort` for those aliases. A user `-m` that already ends in `-low`, `-medium`, or `-high` is sent as-is and `--effort` is not added.

Grok is one model. All four tiers use `grok-4.7` and do not carry an effort. `-e` selects it, default `medium`. The grok runner sends user-facing `max` as `xhigh`. `grok-4.7-build-fast` stays available through `-m`. It is not a tier.

| tier | claude `-m` / `-e` | codex `-m` / `-e` | gemini slug | grok `-m` / `-e` |
|---|---|---|---|---|
| fast | haiku / medium | gpt-6-luna / low | gemini-3.8-flash-low | grok-4.7 / low |
| balanced | sonnet / medium | gpt-6.1-sol / low | gemini-3.8-flash-medium | grok-4.7 / medium |
| smart | opus / high | gpt-6-astra / high | gemini-3.8-flash-high | grok-4.7 / high |
| max | fable / max | gpt-6-astra / max | gemini-4-argon-high | grok-4.7 / `-e`, default medium |

Wrapper defaults when `--tier` and `-m` are both absent:

| provider | model argv | effort argv |
|---|---|---|
| claude | `--model sonnet` | `--effort medium` |
| codex | `-m gpt-6.1-sol` | `model_reasoning_effort="medium"` |
| gemini | `--model gemini-3.8-flash-medium` | no `--effort` flag |
| grok | `-m` omitted (account default, skill says `grok-4.7`) | `--effort medium` |

The user-facing vocabulary is `low|medium|high|max`. The runners map `max` and print a stderr note when the wire value differs:

- claude: `max` stays `max`.
- codex: `max` becomes `ultra`, except model `gpt-6-luna`, which stays `max` because that model has no `ultra`.
- gemini: `max` becomes `high`.
- grok: `max` becomes `xhigh`. For `grok-4.5` it becomes `high`, because that model rejects `xhigh`.

## Wrapper quirks

All four are Python, argparse, an argv list, no shell. Exit 2 is argparse. Exit 124 is the wrapper deadline. Exit 127 is `OSError` from launch, stderr `[<name>] <exc>`. Exit 125 is a logical failure the child may still report as status 0. The answer is buffered until the child exits. The child inherits the environment. Nothing in the wrappers logs in or checks a CLI version.

### claude

```text
claude --output-format json --permission-prompts none <mode>
  [--disallowed-tools WebSearch,WebFetch]
  --model M --effort E [--add-dir D]... [--json-schema S]
  [--resume ID | --continue] -p PROMPT
```

`-C` is `Popen`'s cwd. Child stdin is `/dev/null`.

- default / `--auto`: `--dangerously-skip-permissions`
- `--write`: `--permission-mode auto`
- `--read-only`: `--permission-mode dontAsk`, `--tools` and `--allowed-tools` both set to `Read,Glob,Grep,WebSearch,WebFetch` (`--no-web` drops the `Web*` names), `--strict-mcp-config`, and the fixed read-only system prompt from the script. No sandbox and no second launch.
- `-r` and `-c` together is exit 2.

Success is child status 0, JSON object, `type=="result"`, `subtype=="success"`, `is_error` falsy, answer text nonempty, and `permission_denials` empty. A nonempty denial list is 125 even when `subtype` is `success`. Denial names come from `tool_name`, de-duplicated in order. `stop_reason` is not read. Empty `result` is 125. A good result returns the child status unchanged, so child status 1 with `subtype=success` stays 1.

`--schema` prints `structured_output` when it is present (a string as-is, otherwise pretty JSON), else the `result` string.

Timeout: `start_new_session`, `SIGTERM` the group, 5 seconds, `SIGKILL`. Exit 124 drops the captured output, including the session id. A caught SIGINT/SIGTERM is `128+signum` unless the success check then rewrites it to 125.

Session field is `session_id`, printed only after JSON parsed, including on 125, not on timeout.

Usage line on stderr when present: `[claude-run] in=… out=… turns=… cost=$…`. crew prints the same shape with a `crew` prefix.

### gemini

PTY via `pty.openpty()`. The slave is the child's stdin, stdout, and stderr. The parent never writes the prompt to the master. A pipe is the bug this exists to avoid (agy drops stdout off a terminal).

```text
agy -p PROMPT --print-timeout <max(10, timeout-10)>s
  [--model SLUG] [--effort E] [--agent NAME]
  [--conversation ID] [-c]
  [--mode accept-edits | --dangerously-skip-permissions]
  [--sandbox] [--add-dir D]...
```

- default / `--auto`: `--dangerously-skip-permissions`
- `--write`: `--mode accept-edits`
- `--read-only`: neither flag. agy then applies `permissions.allow` itself. crew does not read that settings file.
- `-r` and `-c` are both forwarded. `--print-id -r ID` echoes that id and does not look at the filesystem. Otherwise the id comes from `$ANTIGRAVITY_CLI_HOME` (default `~/.gemini/antigravity-cli`) `cache/last_conversations.json` keyed by `realpath` of the cwd, else the newest `.db` or `.pb` under `conversations/` whose mtime changed during the run.

Stdout is the PTY bytes, UTF-8 with replacement, ANSI stripped with the script's pattern, `\r` removed, `strip`, then exactly one newline. Empty output is still a newline and can exit 0.

Denial: child status exactly 0 and the output matches `no output produced — a tool required the "(\w+)"` (em dash). Exit 125. The denial sentence stays on stdout. A hyphenated tool name does not match, same as the script.

`timeout waiting for response` in the output with a nonzero child status is 124.

There is no schema and no JSON mode.

The script only `kill()`s the direct child, so grandchildren survive a timeout. crew starts the child in a new session and signals the group, SIGTERM then 5 seconds then SIGKILL, same as claude. That is the one intentional change. The PTY, the denial regex, the `--print-timeout` clamp, and the exit rewrite stay.

### grok

```text
grok -p PROMPT --no-auto-update --output-format json --always-approve <mode>
  [-m MODEL] --effort E [--cwd DIR] [--max-turns N]
  [--json-schema S] [--disable-web-search]
  [--resume ID | --continue]
```

`-C` is grok's `--cwd`. The crew process cwd stays put. Child stdin is `/dev/null`. `-r` wins over `-c` with no error. `-m` is omitted when the user passed neither `--tier` nor `-m`.

- default / `--auto`: `--always-approve`, no sandbox
- `--write`: `--sandbox workspace`, no retry
- `--read-only` attempt 1: `--sandbox read-only` plus the kernel read-only rule string
- retry once when `--read-only`, the deadline did not fire, the child status is not 0 and not 124, and stderr contains `sandbox could not be applied` or both `could not apply the` and `sandbox profile`. The first stdout is dropped. Stderr keeps one sandbox line. crew prints `[crew] read-only sandbox did not start (<detail>). Using a read-only tool allowlist with no shell.` Attempt 2 has no sandbox, `--tools read_file,grep,list_dir,web_search,web_fetch` (`--no-web` drops the `web_*` names), `--no-subagents`, `--deny MCPTool`, and the no-shell rule string.

`stopReason` is normalized by lowercasing and keeping alphanumerics, so `end_turn`, `EndTurn`, `end-turn`, and `end turn` are `endturn`. Exit 125 only when the field is present and is not `endturn`. A missing `stopReason` is success. `--raw` skips this check. This is narrower than the skill sentence, and the script is right.

Hints on 125: normalized `cancelled` mentions a blocked tool (and the read-only limit when that mode was set). A value starting with `maxturn` mentions `--max-turns`. `canceled` with one l is still 125 but gets no cancelled hint.

Empty stdout with child status 0 exits 0 and prints no answer line. `type=error` is exit 1 (or the child status if it is already nonzero) and prints `[crew] error: <message>`.

Schema: print `structuredOutput` when present. If `--schema` was set and `structuredOutput` is absent, scan `text` for the last JSON object and pretty-print it.

Timeout matches claude: group SIGTERM, 5 seconds, SIGKILL, output dropped, and a timed-out first attempt is not retried. Session field is `sessionId`.

On this Mac the read-only sandbox often refuses `/var/run/docker.sock` because it is a symlink. The retry is the path that actually runs. Tests feed the refusal string in and assert the second argv. They do not need a live grok.

### codex

```text
codex exec -c approval_policy="never"
  <--dangerously-bypass-approvals-and-sandbox | -s read-only | -s workspace-write>
  [--add-dir ABS]... [resume | review]
  --json --skip-git-repo-check -m MODEL
  -c model_reasoning_effort="EFFORT"
  [--output-schema ABS] [--last | RESUME_ID]
  [--uncommitted | --base BRANCH | --commit SHA]
  [-- PROMPT]
```

`-C` is `Popen`'s cwd. `approval_policy="never"` is set in every mode. Default / `--auto` is `--dangerously-bypass-approvals-and-sandbox`. `--read-only` is `-s read-only`. `--write` is `-s workspace-write`. No tool list and no retry.

Success: saw `turn.completed`, did not see `turn.failed` anywhere (a later `turn.completed` does not save it), and the last `agent_message` text is nonempty after `strip`. `error` events are copied to stderr and do not by themselves fail a later completed turn. Positive child status is returned as-is and skips the 125 check. A negative child status becomes `128+N`. There is no permission-denial string match.

The answer is still printed on 125 and on 124. `--print-id` uses the last `thread.started.thread_id` and is printed before the answer, including on failure. `-c` is `resume --last`. `-r` is `resume <id>`. `-r` and `-c` together is exit 2.

Timeout: `start_new_session`, `SIGKILL` the group immediately, no SIGTERM grace. Captured output is still printed. That hard kill stays. Codex is the wrapper that already does it.

## Job store

`$CREW_HOME` or `~/.local/share/crew`. Never baton's directory.

Per job: `run.json`, `stdout`, `stderr`. `resume.lock` is only the flock. No pid files.

`run.json` uses serde `deny_unknown_fields`. Status is not stored. It is derived, so a dead supervisor cannot look successful:

- no exit code, supervisor alive → `running`
- no exit code, supervisor gone → `lost`
- `stopped == true` → `stopped`
- exit 0 → `ok`
- exit 124 → `timeout`
- exit 125 → `unclean`
- anything else → `failed`

Fields: `name`, `provider` (`claude|gemini|grok|codex`), `args` (passthrough flags only; crew owns the prompt, `--print-id`, `-r`, `-c`, `-C`), `cwd` (absolute), `note`, `parent`, `resume_session`, `created_at`, `started_at`, `finished_at`, `supervisor_pid`, `supervisor_start` (`ps -o lstart=`), `wrapper_pid`, `session_id`, `exit_code`, `stopped`, `usage` (optional `input_tokens`, `output_tokens`, `total_tokens`, `turns`, `cost_usd`). Name matches `^[a-z0-9][a-z0-9._-]{0,63}$`. PIDs are in `1..=2^22`. Timestamps are UTC.

Open the record with `O_NOFOLLOW|O_NONBLOCK` and `fstat` it. Refuse a symlink or a fifo. A live supervisor is the pid plus the stored `lstart`.

Supervisor: detached subprocess with a new session, provider child in its own process group. The prompt crosses a length-prefixed pipe. `stop` sends `SIGTERM` to the group, waits 10 seconds, then `SIGKILL`. The stored exit is the raw status (143 or 137). `wait` returns 130 when the derived status is `stopped`.

Resume: exclusive flock, child `<base>.<N>` starting at `.2`, same `cwd`, parent's `session_id` as `-r`.

## Usage sources

Same sources as `~/usagebar/usage.py`. Paths and JSON keys only. Never log the values.

- Claude: a PTY, `claude --allowed-tools ""` plus the probe's other args, scratch dir `~/Library/Application Support/UsageBar/ClaudeProbe`, the same trust-prompt replies, write `/usage`, parse the screen buffer for the 5h and week percents. No credential file.
- Codex: bearer token at `tokens.access_token` in `~/.codex/auth.json`. GET `https://chatgpt.com/backend-api/wham/usage`.
- Grok: the OIDC `key` field in `~/.grok/auth.json`. GET `https://cli-chat-proxy.grok.com/v1/billing`. On 401, 403, or an expiring token, run `grok models` and reread the file.
- Gemini: TCP connect to `oauth2.googleapis.com:443` with a 3 second timeout. Then argv `agy -p /usage --output-format json`, cache 120 seconds. usagebar runs that argv with `subprocess` and no PTY, which is the stdout-drop bug. crew uses the same argv and the same cache, on the gemini PTY helper.

## Review behavior

Store stays `~/.local/share/reviewsubagent/reviews.jsonl` so today's history is still the picker input. Cap 30. Atomic replace. Fields: `when` (UTC), `model`, `task`, `what_they_did`, `duration_s`, `cost`, `score`, `reviewer`.

Candidates are only models that already appear in the log. The judge is a random choice of claude/`haiku`, gemini/`gemini-3.8-flash`, or codex/`gpt-6-luna`, at effort high, timeout 300, `--read-only`, invoked as `crew <provider>` rather than the old `*-run` binary. The prompt still gets the benchmark blurb, the usage text, the candidate list, the history, and the task, and it still requires exactly one candidate id. A judge that fails to launch is skipped. The other two are tried. Exit 2 if none returns a candidate. Codex being out of quota is handled by that retry, not by a special case.

## Tests

Under 40. No live network and no live agent in `cargo test`.

- Tiers: four rows per provider, user-file override, unknown key rejected, `-m` overrides the model, omitted tier uses effort `medium`, gemini alias does not also emit `--effort`.
- Argv and exit tables: claude denial list, claude empty result, gemini denial regex and the timeout-waiting rewrite, grok `stopReason` normalization including a missing field, grok sandbox retry when the refusal string is in stderr and not when status is 0, codex `turn.failed` after a later `turn.completed`, codex `max` becomes `ultra` except on `gpt-6-luna`, exit 124/125/127 selection.
- `long_run_note` is absent at 20 minutes and present above it.
- Job: bad name, unknown JSON field rejected, every `status_of` arm, resume suffix, `wait` returns 130 while the file holds 143.
- Usage render: width, one partial block, sort by least-used window, the grok mark is not an error. Parsers take fixtures.
- Load: format function on a fixed sample, including swap and the `crew` line.
- Review: duration, cost, score bounds, the 30-item cap, pick rejects an id outside the candidate set.

## Out of scope

- Editing `claude-run`, `codex-run`, `agy-run`, `grok-run`, baton, usagebar, subagentcounter, reviewsubagent, dotfiles, or `PATH`. `MIGRATION.md` only proposes later shims.
- Importing baton job directories.
- A Linux PTY. `crew load` may keep the script's `/proc` numbers, but the tested machine is this Mac. GPU stays IOKit.
- GUI, installers, completions, a man page.
- Moving the review benchmark prose into a new config file.
- Calling Codex while its week is exhausted. The codex runner is still implemented and covered by argv tests.

## Done

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, tests green. README quick start plus the rejected names. `MIGRATION.md` maps each old command onto `crew`. `docs/demo.md` is one real `--tier fast` run per provider that has credits. Codex is omitted from the demo while its week is at 100%, and the transcript says so.
