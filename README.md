# crew

One Rust CLI over the agent runners (`claude`, `codex`, `agy`, `grok`), named background jobs, machine load, and provider credit usage.

## Name

The command is `crew`.

- `crew` — chosen. `crew claude`, `crew job ls`, and `crew load` are plain English. Not on `PATH`. No Homebrew formula.
- `roof` — the working title. Also clear of `PATH` and Homebrew. Rejected as a metaphor, which is the same failure as `baton`.
- `span` — one binary spanning the tools. Rejected as less obvious.

`desk`, `aide`, and `nest` were dropped because they are Homebrew formulas.

## Quick start

```sh
cargo build
./target/debug/crew models
./target/debug/crew load
./target/debug/crew usage

./target/debug/crew grok --tier fast "Reply with the single word pong"
./target/debug/crew claude --tier fast "Reply with the single word pong"
./target/debug/crew gemini --tier fast "Reply with the single word pong"
```

`--tier` is `fast`, `balanced`, `smart`, or `max`. `-m` overrides that row's model. `-e` is `low`, `medium`, `high`, or `max`, and the default is `medium`. A grok tier does not select effort; `-e` does, and `crew models` prints `-` in that column.

Omitting the mode flags is full autonomy. `--auto` is the explicit spelling of that and is otherwise ignored. `--read-only` and `--write` are the other modes. `-t` is seconds and defaults to 1800.

`~/.config/crew/tiers.toml` replaces the built-in table. `CREW_TIERS` overrides that path. Jobs live in `$CREW_HOME` or `~/.local/share/crew`.

```sh
./target/debug/crew job run demo grok --tier fast "Reply with the single word pong"
./target/debug/crew job wait demo
./target/debug/crew job result demo
```

A finished run longer than 20 minutes prints one stderr line telling the orchestrator to rate it later with `crew review write`. crew does not file that review.

The plan is [PLAN.md](PLAN.md). A real run of each provider that had credits is in [docs/demo.md](docs/demo.md). Nothing here replaces the old tools; see [MIGRATION.md](MIGRATION.md).
