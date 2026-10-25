# crew

One binary over Claude, Gemini (`agy`), Grok, Codex (`gpt`), and pi.

Start with `crew models` for the tier table, then `crew <provider> --tier fast "..."`. Every run has full autonomy.

## Install

Runs on macOS and Linux. Needs a Rust toolchain and at least one of the `claude`, `agy`, `grok`, `codex`, or `pi` CLIs, logged in.

```console
cargo install --git https://github.com/Vl4dk0/crew --locked
```

`crew update` later updates the installed CLIs and crew itself.

## Quick Start

```console
# View intelligence tier mappings across providers
crew models

# Run an agent in the foreground
crew grok --tier fast "Reply with the single word pong"

# Check system telemetry (CPU, RAM, GPU, running crew agents)
crew load

# Check subscription plan quota meters and reset times
crew usage

# Update each installed CLI, then crew
crew update
```

Run `crew help <cmd>` or `<cmd> --help` for details.

## Contributing

Enable the format-on-commit hook: `git config core.hooksPath .githooks`
