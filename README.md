# crew

One unified Rust CLI over Vladko's AI agent runners (`claude`, `codex`, `agy`/`gemini`, `grok`), durable background jobs, macOS machine load telemetry, and provider subscription credit usage.

The binary name is **`crew`**.

Alternatives considered and recorded:
- `roof`: The initial working title; dropped because it describes a position rather than active workers.
- `span`: Explored as a structural metaphor; less intuitive than a team of agents.
- `baton`: Existing tool name; unclear and associated with relays rather than autonomous execution.
- `desk`, `aide`, `nest`: Dropped because they conflict with existing Homebrew formulas or macOS commands.

---

## Quick Start

### 1. Direct Agent Runs
Invoke the provider directly as the subcommand. Every run has full autonomy, and crew writes `SESSION_ID=<id>` to stderr when the CLI reports one; pass it to `-r` to resume.

```console
# Fast run using intelligence tiers
$ crew gemini --tier fast "Fix typo in src/main.rs"

# Explicit model and effort override
$ crew claude -m sonnet -e high "Refactor error handling"

# Grok execution (effort set via -e, default medium)
$ crew grok --tier smart -e low "Summarize latest git log"

# Review git diff using Codex
$ crew codex --review --uncommitted
```

Aliases supported:
- `gemini`: `antigravity`, `agy`
- `codex`: `gpt`, `chatgpt`

### 2. Intelligence Tiers (`crew models`)
Map semantic tiers (`fast`, `balanced`, `smart`, `max`) without remembering exact provider model slugs:

```console
$ crew models
provider  tier      model                      effort  source
claude    fast      haiku                      medium  builtin
claude    balanced  sonnet                     medium  builtin
claude    smart     opus                       high    builtin
claude    max       fable                      max     builtin
codex     fast      gpt-6-luna                 low     builtin
codex     balanced  gpt-6.1-sol                low     builtin
codex     smart     gpt-6-astra                high    builtin
codex     max       gpt-6-astra                max     builtin
gemini    fast      gemini-3.8-flash-low       low     builtin
gemini    balanced  gemini-3.8-flash-medium    medium  builtin
gemini    smart     gemini-3.8-flash-high      high    builtin
gemini    max       gemini-4-argon-high        high    builtin
grok      fast      grok-4.7                   -       builtin
grok      balanced  grok-4.7                   -       builtin
grok      smart     grok-4.7                   -       builtin
grok      max       grok-4.7                   -       builtin
```

### 3. Durable Background Jobs (`crew job`)
Supervisor-monitored detached background execution with state stored under `$CREW_HOME` (default `~/.local/share/crew/runs/<name>/`):

```console
# Start a detached job (streams prompt to child stdin)
$ crew job run audit claude -- --tier fast "Audit dependencies for vulnerabilities"

# Check active and completed jobs
$ crew job ls
NAME            STATUS    WRAPPER     MODEL                    AGE  
audit           running   claude      haiku                    12s  

# Tail stderr logs
$ crew job log audit -n 50

# Wait synchronously for completion
$ crew job wait audit

# Read stdout safely (with symlink verification)
$ crew job result audit

# Resume an existing session under an incremented name (audit.2)
$ crew job resume audit "Check Cargo.lock too"

# Stop a running job gracefully (SIGTERM, 10s grace, SIGKILL)
$ crew job stop audit
```

### 4. Hardware Load Telemetry (`crew load`)
Instant macOS hardware metrics (CPU, used/total RAM via Mach page calculation, swap, GPU via IOKit) and running crew agents counted per provider. Providers with no running agents are omitted:

```console
$ crew load
CPU 11% (load 2.1 / 10 cores) | RAM 19.0 / 24.0 GB (79%), swap 0.7 GB | GPU 26%
gemini: 1 | 79810
grok: 2 | 75237; 79803
```

### 5. Subscription Quota Meters (`crew usage`)
Quota bars (20-column eighth-block meters, least-used window first), and the plan. A figure appears only when its source states it; what a provider does not report is left out, never estimated or read from config. No credential is printed or logged.

```console
$ crew usage
⚛ codex  Pro
  week                   0%  ░░░░░░░░░░░░░░░░░░░░  resets Oct 29 22:21

✳ claude  Pro $20/mo
  5h                    23%  ████▌░░░░░░░░░░░░░░░  resets Oct 23 01:49
  week                  55%  ███████████░░░░░░░░░  resets Oct 27 12:59

✦ antigravity
  gemini week           59%  ███████████▊░░░░░░░░  resets Oct 24 12:45
  gemini 5h              4%  ▊░░░░░░░░░░░░░░░░░░░  resets Oct 23 03:00
  other models week      0%  ░░░░░░░░░░░░░░░░░░░░  resets Oct 29 22:21
  other models 5h        0%  ░░░░░░░░░░░░░░░░░░░░  resets Oct 23 04:21
  Gemini 4 Argon week   99%  ███████████████████▊  resets Oct 26 17:50

✕ grok  SuperGrok Heavy
  week                 100%  ████████████████████  resets Oct 23 08:26
```

Where each number comes from:

| | Quota bars and resets | Plan |
| :-- | :-- | :-- |
| claude | Claude Code's stream-json `get_usage` control request: exact utilization and ISO reset for 5h, week, and opus/sonnet week when the plan has them | `organizationType` in `~/.claude.json`; Max is `Max 5x` ($100) or `Max 20x` ($200) by its rate-limit tier, Pro is $20 |
| codex | ChatGPT usage API (`/backend-api/wham/usage`) | `chatgpt_plan_type` in the local login; Plus $20, Go $8. `Pro` has no price: the login does not say if it is the $100, $200, or $500 tier |
| antigravity | `agy -p /usage` | none: `agy` does not report a plan |
| grok | xAI billing API | `subscription_tier_display` in `~/.grok/settings_cache.json`, name only |

A probe that fails shows its error; nothing is cached or estimated in its place.

### 6. Autonomous Model Reviews (`crew review`)
Records, queries, and autonomously selects benchmark review models from historical scores (`~/.local/share/reviewsubagent/reviews.jsonl`, capped at 30 entries):

```console
# Record a review entry
$ crew review write --model opus --task "refactor" --what "Clean async loop" --duration 120 --cost 0.04 --score 5 --reviewer claude

# Query reviews by harness
$ crew review query claude

# Autonomously pick best judge model based on scores and available usage
$ crew review pick "code review"
```

---

## Architectural Highlights & Wrapper Quirks Preserved

- **PTY Execution for Gemini/Antigravity**: Runs `agy` within a pseudo-terminal (`libc::openpty`), bypassing the bug where `agy -p` drops output under non-PTY pipes.
- **Control-protocol Probe for Claude Code**: Asks `get_usage` over Claude Code's stream-json stdin/stdout with isolated settings (no tools, MCP, hooks, or session file). If it fails, `crew usage` shows the error.
- **Process Group Isolation**: Each runner and supervisor child lives in its own process group (`setpgid(0, 0)`), ensuring timeouts and cancellations clean up all descendants.
- **Denial & Failure Detection**: Matches wrapper exit codes:
  - Exit 75: Usage, quota, rate, or credit limit hit
  - Exit 124: Timeout
  - Exit 125: Tool denial or empty output
  - Exit 127: Binary missing or spawn failure
  - Exit 130: Process terminated by SIGTERM / SIGINT
- **Symlink Protection**: Run store files (`run.json`, `stdout`, `stderr`, `resume.lock`) are opened with `O_NOFOLLOW | O_NONBLOCK` and verified via `fstat` as regular files.
- **Civil Calendar Math**: Timestamp calculations implement Howard Hinnant's algorithm to eliminate leap-year and day-of-year drift.
- **Zero Hacks & Minimal Flags**: Validated inputs, `< 100` targeted unit tests, zero warnings on `cargo clippy --all-targets -- -D warnings`.

---

## Documentation Links

- [PLAN.md](PLAN.md): Specification, CLI surface agreement, and architecture layout.
- [MIGRATION.md](MIGRATION.md): Proposed shims for replacing legacy shell wrappers.
- [docs/demo.md](docs/demo.md): Recorded execution transcripts from live test runs.
