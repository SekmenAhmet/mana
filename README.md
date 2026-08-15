# mana

[![CI](https://github.com/SekmenAhmet/mana/actions/workflows/ci.yml/badge.svg)](https://github.com/SekmenAhmet/mana/actions/workflows/ci.yml)

An orchestrator for AI coding-agent CLIs (Rust, TUI). One agent takes the
**PM role** — it plans, talks to you, and never writes code — and the work is
dispatched to sub-agents running on whichever (CLI × model) is cheapest and
available, so expensive quota is spent on judgment and cheap quota on volume.

Nothing about a specific CLI lives in the code: what differs between them is
data in `catalog/*.toml`, embedded in the binary and overridable per machine
via `~/.mana/catalog.local.toml`.

## Build

    cargo build --release

## Usage

    mana install                 # register the catalogued CLIs found on this machine
    mana doctor                  # check the configuration
    mana launch claude           # run Claude Code as the PM, in mana's TUI

`mana install` offers exactly the CLIs the catalogue knows — a CLI with no
entry has no spawn flags, no failure signatures and no PM driver, so
registering it would only put a name in the PM's `list_agents` that every
dispatch then fails on. Add one by dropping an entry in
`~/.mana/catalog.local.toml` and it shows up in the selector like any other.

Sub-agents are never launched from a shell: the PM dispatches them through
mana's own tools, which is what lets mana pick the (CLI × model), own the
worktree and observe the run.

In the TUI: type to talk to the PM, `Enter` to send, `Ctrl+G` for the graph
pane, `Ctrl+C` to quit. `Esc` does nothing — it is the interrupt key of the
agent CLIs themselves, and quitting on it was a v1 mistake.

## Manual QA checklist (v2, requires a real `claude` install)

Everything below is covered by `cargo test` except what only a paid CLI can
answer: whether the flags mana passes are the flags claude honours, and
whether the PM actually behaves like one. That is what this checklist is for.

1. `cargo run -- install` — the list offered is the catalogue's
   (`claude`, `agy`, `copilot`, `opencode`). Pick `claude`; confirm it lands in
   `~/.mana/config.toml` with a real version, path and `version_args`.
2. `cargo run -- doctor` — no issues reported.
3. From a scratch **git** project directory, run `cargo run -- launch claude`.
   Before typing anything, confirm:
   - `~/.claude/skills/mana-pm/SKILL.md` exists and matches
     `assets/roles/pm/SKILL.md` (rewritten on every launch);
   - `~/.mana/projects/<dirname>/mcp-config.json` names mana's **own binary**
     by absolute path and passes `--project-root <that directory>`;
   - `~/.mana/projects/<dirname>/{tasks,logs,reviews}` exist;
   - the PM greets you in the chat pane within a few seconds — not as dimmed
     `·` lines. Dimmed lines mean the catalogue's `[pm.events]` paths no longer
     match claude's stream (degraded on purpose, never silent).
4. Ask: *"list the agents you can dispatch to"*. The PM must call
   `list_agents` and answer with the CLIs, their models, cost classes and
   counters. If it says it has no tools, the MCP registration did not take.
5. Ask: *"ask the PM to try editing a file itself"* — e.g. *"just write the
   fix yourself in src/main.rs"*. It must refuse or fail at the tool layer:
   `[pm].permission_args` in `catalog/claude.toml` allowlists only mana's
   tools plus Read/Grep/Glob. A PM that succeeds in editing means that flag is
   wrong, which is the one thing here that no test can catch.
6. Ask for a trivial task (*"add a `hello.txt` file containing `hi`"*).
   Confirm the PM calls `create_task` then `launch_subagent`, and that it
   reports back roughly one line, not a narration of every tool call.
7. Press `Ctrl+G`. The graph pane shows one node per dispatch:
   `◉ [EXE] claude/haiku <task>` while it runs, `○` once done.
8. When the executor finishes, confirm the PM reacts on its own — mana injects
   `[mana] executor finished for task …` into the session (visible as a cyan
   `*` line), and the PM should launch a reviewer without being asked.
9. Once the reviewer lands, the node shows `✅` (or `❌` on a rejection), and
   `~/.mana/projects/<dirname>/reviews/<task>.json` holds the verdict.
10. `Ctrl+C`. Confirm the terminal is restored and that **no `claude` process
    survives** (`ps aux | grep claude`) — neither the PM nor the
    `mana mcp-server` it had spawned.

The walkthrough above is written for `claude` (the `stream` driver, tools over
MCP). The same steps apply to the other catalogued CLIs — `agy`
(`oneshot-continue`, tools over the sentinel channel), `copilot` and
`opencode` (ACP) — with the differences the catalogue declares: step 5 has
nothing to check where `[pm].permission_args` is empty, and on the sentinel
channel step 4's tool call appears as a fenced ```mana block in the transcript
rather than as an MCP call.
