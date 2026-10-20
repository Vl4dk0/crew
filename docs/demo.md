# Demo

Recorded on 2026-10-20 from `./target/debug/crew` at main `13b3a21`. Each prompt was one cheap `--tier fast` run with `-t 300`. Codex was not run: `usagebar` showed its week window at 100%.

`crew models` prints `-` for grok effort. `-e` sets that, and the default is `medium`.

```
$ ./target/debug/crew models
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

## Machine Load (`crew load`)

```
$ ./target/debug/crew load
load: CPU 12% (load 3.4 / 10 cores) | RAM 18.9 / 24.0 GB (79%), swap 0.7 GB | GPU 19%
claude-run: 0
agy-run: 2 | 39496; 57220
codex-run: 0
grok-run: 2 | 75237; 79803
crew: 0
```

## Quota Usage (`crew usage`)

```
$ ./target/debug/crew usage
✕ grok
  week                  15%  ███░░░░░░░░░░░░░░░░░

✦ antigravity
  gemini week           40%  ████████░░░░░░░░░░░░
  gemini 5h             35%  ███████░░░░░░░░░░░░░
  other models week      0%  ░░░░░░░░░░░░░░░░░░░░
  other models 5h        0%  ░░░░░░░░░░░░░░░░░░░░
  Gemini 4 Argon week   61%  ████████████▏░░░░░░░

✳ claude
  5h                    84%  ████████████████▊░░░
  week                  19%  ███▊░░░░░░░░░░░░░░░░

⚛ codex  Pro
  týždeň               100%  ████████████████████  stale
```

## Direct Agent Runs

```
$ ./target/debug/crew grok --tier fast -t 300 "Reply with the single word pong and nothing else."
pong
[crew] tokens=21655 turns=1 cost=$0.00977908
```

Exit 0.

```
$ ./target/debug/crew claude --tier fast -t 300 "Reply with the single word pong and nothing else."
pong
[crew] in=2 out=4 turns=1 cost=$0.0085058
```

Exit 0. The tier row is `haiku` at effort `medium`.

```
$ ./target/debug/crew gemini --tier fast -t 300 "Reply with the single word pong and nothing else."
pong
```

Exit 0. The tier row is the slug `gemini-3.8-flash-low`, so no separate `--effort` was sent.

## Background Jobs (`crew job`)

```
$ ./target/debug/crew job run demo gemini -- --tier fast -t 60 "Reply with the single word pong and nothing else."
demo

$ ./target/debug/crew job wait demo
$ ./target/debug/crew job result demo
pong

$ ./target/debug/crew job show demo
run: demo
status: ok
provider: gemini
args: ["--tier", "fast", "-t", "60"]
cwd: /Users/vladko/Projects/crew
created_at: 2026-10-20T20:30:21.889354Z
started_at: 2026-10-20T20:30:21.895539Z
finished_at: 2026-10-20T20:30:26.844688Z
exit_code: 0
session_id: f549226f-b811-4e93-9d4f-92d734c81881
```

Exit 0.
