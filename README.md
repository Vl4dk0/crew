# crew

One binary over Claude, Gemini (`agy`), Grok, and Codex (`gpt`).

Start with `crew models` for the tier table, then `crew <provider> --tier fast "..."`. Every run has full autonomy.

## Install

```console
cargo install --path .
```

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

# Update each installed CLI, then crew (a path install is left alone)
crew update
```

Run `crew help <cmd>` or `<cmd> --help` for details.
