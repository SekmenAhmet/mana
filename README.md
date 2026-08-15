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
    mana doctor                  # check the catalogue, this project and the config
    mana launch claude           # run Claude Code as the PM, in mana's TUI
    mana launch claude -c        # ...picking the previous conversation back up
    mana launch -c               # ...on whichever CLI this project used last
    mana ps                      # what has been dispatched, and what became of it
    mana kill <agent-id>         # stop a sub-agent the PM cannot stop itself

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

### Quitting

`Ctrl+C` (and a PM that dies on its own) ends the session **and stops the
sub-agents it had in flight**, through the same machinery as `mana kill`: guard
first, then the process group, then the exit record and the notification. mana
was the only thing watching those runs, so leaving them alive would mean
processes writing into logs nobody reads, holding quota and worktrees, that
`mana ps` calls `running` for ever. What it did is printed after the terminal is
restored:

    mana: killed 2 in-flight agent(s): d4ce69c8, ab1e17d8

A pid the guard refuses is left alone and named, with the reason, because you
now own a process mana would not touch. Only this project is swept: another mana
in another directory has its own agents.

### `mana launch --continue`

    mana launch claude --continue    # or -c
    mana launch -c                   # the CLI this project used last

Resumes the PM conversation instead of starting a fresh one. How that happens is
per CLI and lives in the catalogue: claude appends `--continue` to its argv
(`[pm].resume_args`), agy starts its first turn from `[pm].continue_args`, and
an ACP CLI is asked for `session/load` with the session id mana stored — but
only if its handshake advertises `loadSession`. A CLI that cannot resume
**refuses the launch** and says why, rather than opening a fresh conversation
under a flag that promised the old one.

On resume mana does not re-send the activation: a continued conversation has
already had it, and replaying it (with the whole role text, on the CLIs that
inline it) would cost a large turn to teach a PM what it already knows. It gets
one line instead — *"[mana] session resumed …"* — while the skill file on disk
is still rewritten, because that file is generated output and this binary may be
newer than the one that wrote it.

The last CLI launched, and the ACP session id to resume by, live in
`~/.mana/projects/<project>/state.toml`. It is a cache: delete it and `-c` just
asks you to name the CLI once more.

### Where the PM skill is installed

mana writes `assets/roles/pm/SKILL.md` to the first directory in the CLI's
`[skills].dirs` on every launch. For claude that is now **`.claude/skills/` in
the project**, not `~/.claude/skills`: the role only means anything inside a
mana session, and a global install put it in the skill list of every project you
open (where, by that CLI's own precedence rules, it would also shadow the
project copy). Every *other* directory in that CLI's list has its `mana-pm/`
removed on launch — that is mana cleaning up after its own earlier versions,
and it says so in the chat pane. Nothing but `mana-pm/` is ever touched.

The project-local directory carries a `.gitignore` of its own containing `*`, so
it stays out of `git status` without mana editing the `.gitignore` you wrote.

### `mana ps`

    mana ps                      # this project (the working directory's name)
    mana ps --all                # every project under ~/.mana/projects
    mana ps --project ../my-api  # some other project

    AGENT     ROLE      CLI/MODEL     TASK      PID    AGE  STATUS
    d4ce69c8  executor  claude/haiku  m1-hello  71086  12m  running
    ab1e17d8  reviewer  claude/haiku  m1-hello  75666  5h   done
    daad9367  executor  agy/gemini-3  9d4e4a7b  75899  2d   stale

The status is derived, never stored: `done` means the agent's log carries an
`exited` record, `running` means its pid still answers, and **`stale` means
neither** — a dispatch whose process is gone and which never wrote an exit
record, so nothing will ever finish it and the PM is still waiting on it.
`unknown` appears when there is no pid to ask about (a note under the table
says which case it is). Always exits 0: a listing that fails is one no script
can pipe.

### `mana kill`

    mana kill d4ce69c8           # an unambiguous prefix is enough
    mana kill d4ce --all         # search every project

Kills the whole process group, the way `mana` created it, so a CLI that
backgrounded a helper dies with it. Then it appends the same two records a
normal completion appends — an `exited` line in the agent's log and a line in
`notifications.jsonl` — so `ps` stops calling it running and the PM is told.

Before signalling anything, mana checks that the pid is still plausibly the
dispatch's: a mana sub-agent always leads its own process group, and its
process cannot be younger than its record. A pid that fails either check has
been recycled onto somebody else's process, and the kill is **refused** rather
than downgraded to a warning. That guard is a strong likelihood, not a proof —
a pid recycled onto another group leader within two minutes of the dispatch's
own age would pass it. Killing a pid that is already gone is a clean no-op that
still records the completion, which is how a `stale` row gets cleared.

### `mana doctor`

    mana doctor                  # catalogue, this project, config
    mana doctor --project ../my-api
    mana doctor --prune          # remove worktrees no running dispatch is using

Catalogue-first, because the catalogue is what mana acts on. Per CLI: whether
the binary is on `PATH`, its version, its PM driver and tool channel, its
models (static, or actually discovered by running the CLI's own command), its
quota pools and failure signatures, which pairs are resting on a cooldown right
now, every capability it *lacks* (no auto-approve flag, no permission flags, a
concurrency cap, a cwd it ignores), and the first line of its catalogue notes.
Then the project's counters and verdict tallies, the dispatches still in flight
or stale, leftover worktrees, and the config file.

**Exit codes.** `0` unless something is broken-broken, and exactly three things
count: a *registered* CLI whose binary has vanished, a stale dispatch, or a
config file mana cannot read (including v1's leftover `config.yaml`). A
catalogued CLI you never installed, a failed model discovery, an active
cooldown and a leftover worktree are all reported and all still exit 0. Output
is plain aligned text with no colour, so `mana doctor | grep BROKEN` works.

`--prune` removes worktrees under `~/.mana/worktrees/<project>/` that no
running dispatch is using, and refuses to touch the ones that are.

## Updates

    mana upgrade                 # download and install the newest release

`mana launch` also checks for a newer release in the background and, if there
is one, prints a single line into the chat pane:

    * [mana] mana 0.2.0 available -- run `mana upgrade`

The check never blocks the launch and never fails it — being offline is
normal, and looks like silence. The answer is cached in
`~/.mana/update-check.json` for 24 hours, so it costs at most one request a
day. Set `MANA_NO_UPDATE_CHECK=1` to switch it off. No other command looks:
`ps`, `kill`, `doctor` and the MCP server never touch the network.

Releases are cut by merging `develop` into `main` and then merging the Release
PR a robot opens — see [RELEASING.md](RELEASING.md).

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
