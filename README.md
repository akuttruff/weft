# weft

A coding agent for the terminal that runs two local models in tandem through
[LM Studio](https://lmstudio.ai), in Rust.

- **The planner** (a dense model asked to think hard) writes a short plan and
  later reviews the diff.
- **The worker** (a fast mixture-of-experts model, thinking off) runs the tool
  loop: reads, searches, edits, runs commands, follows the plan.

Built on a Mac Studio with `qwen/qwen3.8-27b` as the planner and
`qwen3.5-122b-a10b` as the worker, but any two models LM Studio serves will do.

## Status

Early. The worker loop works end to end; the planner, the full-screen UI and
MCP servers are next.

```
$ weft run "The slugify tests fail. Fix slug.js so they pass, and run the tests."
⏺ Read slug.test.js        ⎿ 1  import { test } from 'node:test'; (+12 lines)
⏺ Read slug.js             ⎿ 1  // Turn a title into a URL slug. (+3 lines)
⏺ Bash node --test …       ⎿ ✔ lowercases and hyphenates (+58 lines)
⏺ Edit slug.js             ⎿ edited slug.js (+4 −1)
⏺ Bash node --test …       ⎿ ✔ lowercases and hyphenates (+10 lines)
All 3 tests pass. slug.js:2-4: …
5 rounds · 14s · 9708 prompt + 384 completion tokens · touched: slug.js
```

## Two routes to one server

LM Studio exposes reasoning-effort levels only for models whose template
declares them. The 27B has `low / medium / xhigh`, so the planner uses the
OpenAI-style `/v1/chat/completions` route and asks for the level it wants.

The 122B's template has only an on/off switch, and LM Studio does not expose
it, so the worker uses the raw `/v1/completions` route: weft renders the prompt
in Qwen's chat format itself, starts the assistant turn with an empty
`<think></think>` block, and parses the `<tool_call>` XML the model writes.
On a small task that is the difference between a few hundred and a few
thousand generated tokens.

## Setup

```bash
cargo install --path .
mkdir -p ~/.config/weft
cat > ~/.config/weft/config.toml <<'EOF'
[lm_studio]
url = "http://127.0.0.1:1234"
api_token = "..."            # or set LM_API_TOKEN

[planner]
model = "qwen/qwen3.8-27b"
plan_effort = "xhigh"
review_effort = "medium"

[worker]
model = "qwen3.5-122b-a10b"
thinking = false
EOF
chmod 600 ~/.config/weft/config.toml
```

## Commands

```bash
weft run "task"            # the worker does the task in the current folder
weft run -y "task"         # approve every tool call without asking
weft run --think "task"    # let the worker think before each step
weft probe                 # ask both models one question; shows each route working
weft tool search '{"pattern":"fn main","glob":"*.rs"}'   # run one tool by hand
```

Edits, new files and shell commands ask first: `[y] once  [a] always this
session  [n] no`. Every path is resolved against the project root and refused
outside it, symlinks included. `pkill`, `killall` and force pushes are never
run.

## Tools the worker has

| Tool | What it does |
|---|---|
| `read_file` | Numbered lines, up to 200 at a time |
| `list_files` | Folder listing, honours `.gitignore` |
| `search` | Regex over the project, `path:line: text`, capped at 100 matches |
| `edit_file` | Replace one exact passage; must match exactly once |
| `write_file` | New files only |
| `bash` | Runs in the project root, timeout, last 200 lines of output |

## Reading the code to learn Rust

The comments marked `// Rust:` explain each language feature where it first
appears. In order:

1. **`src/lm/`** — enums with data (`Message`, `StreamEvent`), `String::drain`,
   a struct with a lifetime (`Parser<'a>`), async streaming with channels.
2. **`src/tools/`** — the `Tool` trait, `Box<dyn Tool>`, `#[async_trait]`,
   path handling, `tokio::process` with `join!` on both pipes.
3. **`src/agent/`** — the `Model` trait so tests can script a fake model, the
   worker loop, `tokio::select!` for cancellation, a forwarding task.
4. **`src/main.rs`** — clap subcommands, `spawn_blocking` for stdin, generics
   over a closure that returns a future.

## Development

```bash
cargo test
cargo clippy --all-targets
cargo fmt
```
