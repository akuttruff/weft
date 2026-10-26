# <img src="docs/icon.png" width="32" alt=""> Weft

A coding agent for the terminal that runs local models in tandem through
[LM Studio](https://lmstudio.ai). I built it in Rust in order to become better
acquainted with the language.

- **The planner**, a dense model, writes a short plan and later reviews the diff.
- **The worker**, a fast mixture-of-experts model with thinking off, runs the
  tool loop: reads, searches, edits, runs commands, follows the plan.
- **The scout**, a small model, answers the other two's questions about where
  things live and writes the note a long conversation is folded into.

Built with `qwen/qwen3.8-27b`, `qwen3.5-122b-a10b` and `qwen/qwen3.5-9b` in
those roles; any models LM Studio serves will do.

## In the terminal

An input box at the bottom, the transcript above it in the terminal's own
scrollback. Two tasks in a row on a React + Node monitoring board: "sort the
monitor tiles so anything that's alerting or warning comes first in its
group", then "now add a test for that":

![a Weft session: a change, then a test for it](docs/weft.png)

Each `⏺` line is a tool call with the first line of its result. Edits show
as numbered red and green diffs; the plan sits in a cyan gutter. The second
task reads against the first. Edits, new files and shell commands ask first:

![Weft asking before an edit](docs/approval.png)

**Keys**

| Key | Does |
|---|---|
| enter | sends the task |
| shift-tab | cycles how much weft asks: ask → edits auto → all auto → plan first |
| ctrl-j, or `\` + enter | a newline |
| ↑ ↓ | history |
| esc | stops the task |
| y / a / n | answers an approval: once, always this session, no |
| ctrl-c twice | leaves |

The input box uses the Emacs keybindings bash and zsh use: ctrl-a and ctrl-e
for the ends of the line, ctrl-k to cut to the end, alt-b and alt-f by a word.

**Commands** (`/` opens the menu)

| Command | Does |
|---|---|
| `/plan <task>` | plans first and waits for your word on the plan |
| `/go <task>` | straight to work, no plan |
| `/accept` | edits and new files stop asking; shell commands still ask |
| `/undo` | puts back the files the last task changed |
| `/compact` | folds the earlier tasks into a note now |
| `/clear` | starts a fresh conversation |
| `/resume` | lists earlier sessions in this folder; a number picks one up |
| `/restart` | leaves and comes straight back, on the installed build |
| `/think` | toggles the worker's thinking |
| `/memory` | what weft has kept about this project |
| `/cost` | tokens used this session |
| `/mcp` | the MCP servers and their tools |
| `/help`, `/quit` | |

## How a task flows

1. **Project note.** Path, stack, git, the file tree three levels deep, your
   `WEFT.md`, and what weft remembered last time.
2. **Triage.** The worker says in one word whether the task needs a plan.
   `/plan` and `/go` override it.
3. **Plan.** The planner reads with read-only tools, or asks the scout, then
   writes the files and changes, the one command that verifies, and pitfalls.
4. **Work.** The worker follows the plan with the full tool set, runs the
   check, and reports what changed as `path:line`.
5. **Review.** If files changed, the planner model reads the diff and answers
   `APPROVE` or `FIX` with a list. On `FIX` the worker goes again, thinking on.
   One round by default.

Tasks in a session share the worker's conversation, so "now do the same for X"
works.

**Plan mode.** A `/plan` task stops once the plan is on screen. Enter or
"yes" lets the worker go, "no" or esc stops, anything else is notes and the
planner writes the plan again, up to three times. `confirm = "always"` in the
config does this for every plan.

**Undo.** `/undo` puts back the files the last task changed, and the task
before on a second `/undo`. Weft's own edits are always covered; in a git
repository, so is what shell commands changed, from a copy git keeps of the
tree before each task. Your own uncommitted edits survive.

**The scout.** One question about where something lives, answered by a
separate model run that reads the project and replies with paths and line
numbers, never file contents. What it read never enters the worker's window.
A 9B answers in 5–15 seconds; the 122B takes 20–30. Paths in its answer are
checked against the folder, and it is sent back once if it invented one.

**The delegate.** The worker can hand a self-contained piece of the task to a
helper: a fresh worker with the same tools and its own context. Its edits
show on screen, indented, and ask like any other; only its report comes back.
An `a` given to either holds for both. A helper cannot delegate in turn.

## Two routes to one server

The 27B's template declares reasoning-effort levels, so the planner uses
`/v1/chat/completions` and asks for `low`, `medium` or `xhigh`. The 122B's
template has only an on/off switch LM Studio does not expose, so the worker
uses raw `/v1/completions`: weft renders the prompt in Qwen's chat format,
opens the turn with an empty `<think></think>`, and parses the `<tool_call>`
XML itself. On a small task that is a few hundred generated tokens instead of
a few thousand.

## Setup

`weft init` writes a starter config and a `WEFT.md` where they are missing.
At startup weft says if a configured model is not loaded. A misspelt config
key is an error, not a silent default.

```toml
# ~/.config/weft/config.toml  (chmod 600; it holds the token)
[lm_studio]
url = "http://127.0.0.1:1234"
api_token = "..."            # or set LM_API_TOKEN

[planner]
model = "qwen/qwen3.8-27b"
plan_effort = "medium"      # low | medium | xhigh
review_effort = "low"       # one read of the diff; 3x faster than medium
when = "auto"               # auto | always | never
confirm = "asked"           # the plan waits for your word: asked (/plan only) | always | never
review_rounds = 1           # 0 turns review off

[worker]
model = "qwen3.5-122b-a10b"
thinking = false            # the plan carries the reasoning; fix rounds think anyway

[scout]
# model = "qwen/qwen3.5-9b"   # default: the worker's, on the raw route
# effort = "low"              # only for a model that honours reasoning_effort

[compact]
at = 48000                  # fold the conversation at this many tokens; 0 never
window = 65536              # the worker's context window

[vision]
# model = "qwen/qwen3.8-27b"  # looks at images for see_image; default: the planner's
effort = "low"

[approve]                   # tool calls that never ask
# tools = ["edit_file"]
commands = ["cargo test", "cargo build", "git status", "git diff", "git log", "ls"]

[[hooks]]
on = "after_edit"           # after_edit | before_bash | after_task
run = "cargo fmt"           # sh -c, in the project root
```

## Project notes and memory

`WEFT.md` in the project root, and `~/.config/weft/WEFT.md` for every
project, hold your standing instructions: how the tests run, what never to
do, how commits are written. Every model sees them on every task, so keep
them short.

What weft learns on its own goes through its `remember` tool into
`~/.local/share/weft/memory/<folder>.md`, one fact per line, shown after your
notes in this session and the next. `/memory` prints it; edit the file freely.

## Approvals and hooks

Edits, new files, shell commands and MCP tools ask first. `[approve]` in the
config names what never asks: tools by name, shell commands by their leading
words. `cargo test` admits `cargo test --lib`, not `cargo testx`; every part
of a chain must match; anything with `$(…)` or a backtick always asks. The
rules live in your config, never in the repository.

Hooks are your shell commands at set moments. `after_edit` runs with
`WEFT_PATH` set and its output goes to the model with the tool result, so a
formatter or linter speaks at once. `before_bash` runs with `WEFT_COMMAND`
set; a non-zero exit stops the command and its output is the reason the
worker sees. `after_task` runs with `WEFT_TOUCHED` and prints to the screen.

## Custom commands

A Markdown file in `.weft/commands/` is a command: `review.md` is `/review`,
and the file is the task. `~/.config/weft/commands/` holds the global ones; a
project file wins on the same name. Header lines may set a `description` and
a `mode` (`plan`, `go`, `auto`); `$ARGUMENTS` stands for what follows the
command, or the arguments are appended.

```markdown
description: review the working tree before a commit
mode: go

Read `git diff` and list anything a careful reviewer would flag. Do not change files.
```

## MCP servers

`~/.config/weft/mcp.json`, in the shape LM Studio and Claude Desktop use:

```json
{ "mcpServers": { "deep-research": { "command": "node", "args": ["/path/to/index.js"] } } }
```

`command`, `args`, `env` and `cwd` are understood. Servers start with weft over
stdio; the worker sees each tool as `server__tool`, every MCP tool asks first,
and a server that fails is reported and skipped. The planner keeps to the
read-only built-ins. `/mcp` and `weft mcp` show what came up.

## Sessions

Everything a session does goes to
`~/.local/share/weft/sessions/<folder>/<id>.jsonl` as it happens: tasks, the
worker's messages, tool calls, each turn's tokens, plans and reviews. It lives
outside the project, so git never sees it. When you leave, weft prints the
command that picks the session up again.

**Resume.** `weft resume` reopens the last session, `weft resume <id>` an
earlier one, `/resume` lists them inside weft. The screen replays each task
and the last plan, the worker gets its conversation back with a note that the
project may have changed, and the file is appended to from there. Approvals,
the shift-tab mode and undo history belong to the process and start afresh.

**Compaction.** Once the worker's context reaches `[compact] at`, the earlier
tasks are folded into a handover note written by the scout's model: what was
done, files changed, decisions, loose ends. The last task stays verbatim.
`/compact` does it on demand. Inside a task, old tool results are dropped as
the window fills.

## Images

The worker's route carries no images, so `see_image` sends a picture to a
vision model on the chat route and brings back a description or an answer.
Name a screenshot in the task and the worker will look.

## Command line

```bash
weft                        # the interface, in the current folder
weft resume [id]            # the interface, on the last (or the given) session
weft run "task"             # one task, printed line by line (for scripts)
weft run --session last --plan|--go --accept --json "task"
weft -y …                   # approve everything
weft --think …              # the worker thinks before each step
weft sessions               # this folder's sessions
weft tools                  # the worker's tools and what each costs per prompt
weft init                   # a starter config and WEFT.md
weft mcp                    # start the MCP servers and list their tools
weft probe                  # one question to both models, on each route
weft tool search '{"pattern":"fn main"}'   # run one tool by hand
```

Every path is resolved against the project root and refused outside it.
`pkill`, `killall` and force pushes are never run.

## The worker's tools

| Tool | Does |
|---|---|
| `read_file` | numbered lines, up to 200 at a time |
| `list_files` | a folder listing, honouring `.gitignore` |
| `search` | a regex over the project, capped at 100 matches |
| `edit_file` | replaces one exact passage; reports `path:line` |
| `write_file` | new files only |
| `bash` | runs in the project root; last 200 lines |
| `scout` | one where-or-how question, answered by another model |
| `delegate` | one self-contained piece, done by a fresh worker |
| `see_image` | a description of, or an answer about, an image file |
| `remember` | one fact about the project, kept for later |
| `server__tool` | any tool of a configured MCP server |

## Reading the code to learn Rust

Comments marked `// Rust:` explain each language feature where it first
appears. In order:

1. **`src/lm/`** — enums with data, `String::drain`, a struct with a
   lifetime, async streaming over channels.
2. **`src/tools/`** — the `Tool` trait, `Box<dyn Tool>`, `#[async_trait]`,
   path handling, `tokio::process` with `join!` on both pipes.
3. **`src/agent/`** — the `Model` trait and scripted fakes, the worker loop,
   `tokio::select!` for cancellation, forwarding tasks; `tandem.rs` reuses the
   loop for the planner and reviewer; `scout.rs` and `delegate.rs` wrap it as
   a `Tool`, the latter with an `Arc<dyn Approver>` in the context;
   `compact.rs` rewrites a conversation around a note; `convo.rs` owns the
   task message's shape, round-trip tested.
4. **`src/mcp.rs`** — `serde` config, `join_all`, a type alias for a long
   generic, `Arc` sharing one connection, the `Tool` trait for foreign tools.
5. **`src/transcript.rs`** — a tagged `serde` enum as a file format, a
   `Mutex<File>` writer, folding records back into a `Saved` session.
6. **`src/hooks.rs`, `src/commands.rs`, `src/checkpoint.rs`,
   `src/tools/memory.rs`** — one idea each: a `Command` with a timeout and
   environment, a file format with a header, kept copies plus `git`, an
   idempotent append.
7. **`src/models.rs`, `src/session.rs`, `src/ui/`** — config to model handles;
   the session behind an `Arc` with short-held `Mutex`es and an `AtomicU32`;
   the screen as a `tokio::select!` loop split across `events.rs`, `keys.rs`,
   `slash.rs` and `draw.rs` with `impl` blocks in child modules; ratatui's
   inline viewport; approvals as `oneshot` channels; `ui/tests.rs` driving it
   through the key handlers with the test backend.
8. **`src/main.rs`** — clap subcommands, `spawn_blocking` for stdin, `exec`
   for `/restart`.

## Development

```bash
cargo test
cargo clippy --all-targets
cargo fmt
```
