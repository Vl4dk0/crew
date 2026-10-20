# Demo

Recorded on 2026-10-20 from `./target/debug/crew` at main `c7bc562`. Each prompt was one cheap `--tier fast` run with `-t 300`. Codex was not run: `usagebar` showed its week window at 100%.

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
