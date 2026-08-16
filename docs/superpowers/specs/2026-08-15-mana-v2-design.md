# mana v2 — Design

**Date:** 2026-08-15
**Status:** Draft for review — supersedes `2026-08-13-mana-design.md` (architecture
sections) and the whole of `2026-08-15-pty-terminal-emulation-design.md`.
**Origin:** full-day redesign session; audit evidence and decision log in the
vault note `05-Daily/2026-08-15`.

---

## 1. Thesis

Several agent CLIs coexist on a user's machine, each with its own quota pool.
Used one at a time, most of that quota sleeps. **mana spreads the work across
all of them**: one agent takes the **PM role** (plans, talks to the user, never
writes code), and execution is dispatched to sub-agents on whichever
(CLI × model) is cheapest and available — so expensive quota is spent on
judgment, cheap quota on volume.

mana is destined to be public. Users have different CLIs, configs, and
preferences, so nothing may assume a specific CLI — including for the PM role.

## 2. The rule that shapes everything

> **Anything that differs between CLIs must be a field in the catalogue, never
> a branch in the code.** Existing code that violates this changes.

The code contains a small closed set of **generic implementations** (drivers,
channels). The catalogue assigns each CLI one of them, with parameters. Adding
a CLI is a catalogue entry. Only a genuinely new *protocol shape* is new code —
rare, like adding a database driver.

Evidence this matters: Gemini CLI was retired on 2026-06-18 (superseded by
Antigravity/`agy`) while `KNOWN_CLIS` still listed it. CLI knowledge hardcoded
in Rust rots in months. Prior art confirms the cost of the alternative:
vibe-kanban maintains one Rust module per agent; its company shut down in
April 2026 and that per-CLI burden now falls on volunteers.

## 3. Architecture overview

```
┌───────────────────────────── mana (Rust, TUI) ─────────────────────────────┐
│                                                                            │
│  chat pane ◄── PM transport driver ──► PM agent (any CLI, user's choice)  │
│  graph pane      (acp | stream | oneshot-continue)      │                  │
│                                                          │ tool channel    │
│  MCP server (create_task, launch_subagent,  ◄────────────┘ (mcp|sentinel)  │
│              get_review, list_agents)                                      │
│        │                                                                   │
│        ▼ dispatch                                                          │
│  sub-agents: bare spawn + prompt (no protocol) ── in git worktrees ──►     │
│  observer: exit code, duration, files, failure signatures ──► logs.jsonl   │
└────────────────────────────────────────────────────────────────────────────┘
         ▲ embedded catalogue (TOML, build-validated)   ▲ local override
```

Two code paths total, neither CLI-specific:

- **PM role** — an interactive session over a *transport driver*, with mana's
  orchestration tools reachable over a *tool channel*.
- **Sub-agent role** — bare spawn with a prompt; no protocol at all. Works with
  any CLI. This is where quota arbitrage happens; the PM is one session, the
  sub-agents are the volume.

The PTY-mirroring/VT100 approach is dead: an emulator renders the child's
screen *faithfully*, which is the opposite of the goal (show only the
important messages). mana consumes **structured events** and chooses what to
render: assistant text → chat pane; everything else → dropped or logged.

## 4. PM transport drivers

| Driver | Mechanism | Known fits (measured 2026-08-15) |
|---|---|---|
| `acp` | ACP client (JSON-RPC 2.0 over stdio; `initialize`, `session/new`, `session/prompt`, `session/update`, `session/request_permission`) | `copilot --acp`, `gemini --acp`, `opencode acp`, claude via `claude-agent-acp` adapter |
| `stream` | one persistent process, bidirectional JSONL | claude native (`-p --input-format stream-json --output-format stream-json`) — **verified multi-turn: one process, session continuity, clean exit** |
| `oneshot-continue` | one process per turn: headless flag + continue flag + structured output | agy (`--print` + `--continue` + `--output-format stream-json`), most CLIs |

ACP notes: `session/new` accepts an `mcpServers` array, so ACP + MCP wire up in
one handshake. `session/request_permission` lets mana render approval dialogs
in its own TUI. Caveat: implementations are uneven (a Cursor bug report shows
`mcpServers` silently ignored) — verify per CLI before relying on it.

**Thin-contract rule for non-ACP drivers:** the event map (`[pm.events]`)
carries **only `text` (what to display) and `usage` (optional enrichment)**.
Tool calls, permissions, and session control must travel over ACP, MCP, or the
sentinel channel — never be parsed out of a CLI's proprietary stream. This is
the lesson from vibe-kanban: parsing full streams forces per-CLI code. At
runtime, if a path stops matching, fall back to raw text lines — degraded and
visible, never silent; orchestration is unaffected.

> **Amendment (2026-08-16):** a third field, `turn_end` (`{path, equals}`),
> joined the map when the input queue landed. On a persistent-process driver
> the turn boundary exists *only* in the stream, and it is protocol state,
> not content — naming it as catalogue data was judged truer to the deeper
> rule than deriving it invisibly in Rust from a frame that happens to
> coincide. The content prohibition stands: tool calls and permissions still
> never come out of a proprietary stream.

## 5. Tool channels (how the PM orchestrates)

| Channel | Mechanism | Known fits |
|---|---|---|
| `mcp` | mana runs an MCP server (stdio, `rust-mcp-sdk`/`pmcp`); attached via the CLI's own MCP config, an argv template in the catalogue | claude `--mcp-config`, copilot `--additional-mcp-config <json>`, opencode, gemini |
| `sentinel` | the PM emits a fenced JSON block in its text; mana parses it from the structured stream and is the **sole executor** | agy (no MCP surface found) |

Sentinel is safe where v1's `Bash(...)` scraping was not: the double-launch bug
came from the intercepted command *also* really executing. Inert text executes
nowhere; mana is the only actor.

**MCP tools exposed to the PM:**

- `create_task {title, prompt, depends_on?}` → `{task_id}` — mana generates the
  UUID, frontmatter, and path. The PM never writes YAML or knows any path.
- `launch_subagent {task_id, role, model | cost_class}` → `{agent_id}` — with
  `cost_class`, mana resolves the cheapest available (counters + cooldowns)
  deterministically; a hallucinated model id cannot fail a dispatch (validation
  error names valid ids).
- `get_review {task_id}` → `{verdict: validated|rejected, issues[], attribution}`
- `list_agents {}` → installed CLIs, their models with `cost_class`, per
  **(CLI × model)** counters, active cooldowns. Fresh on every call — this is
  why it is a tool and not skill text.

This contract is **internal**: the producer (Rust) and the consumer teaching
(PM skill) ship in the same binary, atomically. Revising it after real runs is
cheap. Only the catalogue carries a `schema` version, because outside humans
edit it.

These tools kill v1's three launch blockers at the root (path mismatch between
prompt and code; double launch; `mana` not on `$PATH` — mana registers its MCP
server via `current_exe()`).

## 6. PM role injection

At `mana launch <cli>`:

1. mana embeds the PM skill content (`include_str!`) and writes it to
   `<catalogue.skills_dir>/mana-pm/SKILL.md` — rewritten every launch, so it
   can never drift from the binary.
2. mana sends one short activation message: *"You are the mana PM for this
   session. Load and follow the mana-pm skill."*

Why this shape: SKILL.md is a cross-vendor standard (~40 tools; identical files
read from the same folder structure, including the vendor-neutral
`~/.agents/skills`); `--append-system-prompt` exists only on claude (1/5);
`--agent` exists on 4/5 but only *selects* a pre-existing agent, which would
force per-CLI creation formats; ACP has no system-prompt concept at all.

Skill style (per Anthropic's authoring guidance): imperative, explain *why*,
no all-caps MUSTs, ~200 lines (measured corpus: median 183, p90 497). Dynamic
data (installed agents, models, counters) is **not** in the skill — it comes
from `list_agents()` so it is never stale mid-session.

The PM's no-code constraint is enforced mechanically where the CLI allows
(e.g. read-only tool sets), stated in the skill otherwise.

## 7. Catalogue

**Embedded in the binary, validated at build time.** A malformed catalogue
cannot ship. No runtime fetch: code and data move together, so there is no
schema-compatibility burden and no supply-chain surface. Updating the
catalogue = commit → release-plz Release PR (batched, cut on the maintainer's
schedule) → cargo-dist builds the 5-target matrix → users get it via
`mana upgrade` (`self_update`, already a dependency) with a soft startup check.
Escape valve for "a CLI died on a Tuesday": `~/.mana/catalog.local.toml`
replaces a CLI's entry **wholesale** (no deep merge), no release needed.

Format: **TOML, one file per CLI in `catalog/`** — hand-curated data needs
comments; the `toml` crate is maintained while serde_yaml is archived
(consequence: `~/.mana/config.yaml` migrates to TOML too). Invocations are
**argv arrays with placeholders** (`{model}`, `{prompt}`, `{config_path}`,
`{cwd}`) — mana substitutes, no shell ever interprets catalogue content.

### Schema (`schema = 1`) — reference entry, real measured flags

```toml
schema = 1

# Top-level, so it must precede the first [table] header — written after one,
# TOML would silently attach it to that table.
notes = "Maintainer notes in English. Never injected into the PM."

[cli]
id           = "claude"
name         = "Claude Code"
bin          = "claude"            # optional [cli.bin_overrides] per OS
version_args = ["--version"]

[pm]
driver = "stream"                  # acp | stream | oneshot-continue
args   = ["-p", "--input-format", "stream-json",
          "--output-format", "stream-json", "--verbose"]
prompt = "stdin-jsonl"             # argv | stdin | stdin-jsonl

[pm.events]                        # non-ACP drivers only; 3 fields MAX (see §4)
text  = "$.message.content[?@.type=='text'].text"
usage = "$.usage"

[tools]
channel  = "mcp"                   # mcp | sentinel
mcp_args = ["--mcp-config", "{config_path}"]

[subagent]
args                  = ["-p", "--output-format", "stream-json"]
auto_approve_args     = ["--dangerously-skip-permissions"]
model_args            = ["--model", "{model}"]
prompt                = "argv"
max_concurrent        = 0          # 0 = unlimited
cwd_required_in_brief = false

[models]
discovery_args = []                # empty = static list only
# line_regex   = '^(\S+)\t'        # capture group = model id

[[models.static]]
id         = "opus"
cost_class = "expensive"           # cheap | mid | expensive — ordinal ONLY
pool       = "plan"

[[quota.pools]]
id         = "plan"
kind       = "tokens"              # requests | tokens | credits | unknown
period     = "5h-window"
pool_scope = "global"              # global | per-model

[[failure]]                        # ordered; first match wins
means            = "rate_limited"  # quota_exhausted | rate_limited | auth_expired
stdout_regex     = "rate.?limit"
cooldown_minutes = 60

[skills]
dirs = ["~/.claude/skills", "~/.agents/skills"]

[install]
url = "https://claude.com/claude-code"
```

### agy — the deltas (every hard-won managent lesson becomes a field)

```toml
[pm]        driver = "oneshot-continue"
            first_args    = ["--print", "--output-format", "stream-json", "{prompt}"]
            continue_args = ["--print", "--continue", "--output-format", "stream-json", "{prompt}"]
[tools]     channel = "sentinel"            # no MCP surface found
[subagent]  max_concurrent = 1             # two parallel dispatches died in 8s
            cwd_required_in_brief = true   # 306k tokens wasted without it
[models]    discovery_args = ["models"]
            line_regex = '^(\S+)\t'
[[quota.pools]] id = "default", kind = "unknown", pool_scope = "per-model"
# no [[failure]] entry: agy has never shown an observable quota signal. The
# (exit 1, empty stderr, "402") signature belongs to copilot's future entry —
# an earlier draft wrongly carried it over here (caught at implementation).
```

**Corrections found while implementing 0.1** (kept here so the schema stays
honest): top-level `notes` must precede the first table header (TOML scoping);
the local override file carries **one** entry in the same format as an embedded
file (overriding several CLIs at once would need a `catalog.local.d/`
directory — deferred); argv templates have **no escape for literal `{`**, which
blocks inline-JSON args (copilot's `--additional-mcp-config`) until an escape
is added with that entry; a `[[quota.pools]]` `limit` field is deferred until a
CLI with a known hard limit (copilot, 200/mo) gets its entry; enum casing is
intentionally as-shipped (kebab-case drivers/scopes, snake_case failure means).

**Deliberately absent from the catalogue:** "what it's good at" prose, model
rankings, $ prices, task categories, observed counters. The first three rot on
someone else's release schedule; the last lives in `~/.mana/` state.

Catalogue CI: schema validation + golden recorded transcripts per CLI replayed
against the `[pm.events]` maps.

## 8. Routing and reputation

**Rule: cheapest quota first; escalate on failure.** No quality judgment is
required or stored — the PM decides *how hard the task is* (cost-class floor),
mana resolves the concrete pair.

**Quota state cannot be queried** — measured across all installed CLIs
(copilot's `/billing`/`/limits` are in-session slash commands; claude only
emits `rate_limit_event` mid-run; `opencode stats` is historical; agy has
nothing). So the router **observes and remembers**:

- Per (CLI × model): dispatches, validated, rejected-for-code, quota failures,
  avg duration. **Raw counters, never a synthesized score** — with dozens of
  cells and few tasks, "4 of 6" lets the PM discount small samples; a "7/10"
  from different sessions cannot stay calibrated.
- The reviewer's verdict *is* the outcome signal, and it carries an
  **attribution field: code vs brief**. Only code-rejections count against the
  model (a bad brief must not condemn a good model — the agy-cwd incident).
- Quota exhaustion is detected by matching exit code + stderr/stdout against
  the catalogue's ordered failure signatures → cooldown on that pool
  (respecting `pool_scope`).
- Requests are counted, not dollars: subscription quotas are request pools and
  mana knows its own dispatch count for free. Cost/token enrichment is
  harvested only where the catalogue declares structured output.

Log field names follow **OTEL GenAI semantic conventions** (`gen_ai.usage.*`,
`gen_ai.client.operation.duration`) so a future OTEL backend is a pipe change.
OTEL itself is rejected for now (experimental, 2/5 CLIs, would force an OTLP
collector into a CLI binary).

## 9. Roles, permissions, isolation

`role` — not a task taxonomy — determines permissions and isolation. A closed
category list was considered and rejected: real tasks don't fit fixed boxes,
and `role` already carries the write/read distinction.

| Role | Writes? | Isolation | Permissions |
|---|---|---|---|
| `executor` | yes | **git worktree** (one per task) | auto-approve flags from catalogue |
| `reviewer` | no | none needed | read-only where the CLI supports it (e.g. `gemini --approval-mode plan`) |

Worktrees: the 2026 standard for parallel agents (native in Claude Code, Codex,
Cursor; ~8–10 concurrent is the practical ceiling; no symlinks needed on
Windows → no admin rights; enable `core.longpaths` on Windows; `git worktree
prune` on cleanup is mandatory — managent already hit stale locks). Runtime
isolation (ports, DBs) is out of scope for v2; containers can layer on later.

**Non-git projects:** write roles require a git repo — without one, mana
refuses the dispatch with a clear message ("init a repo first"). Read-only
roles work anywhere. No degraded write mode to maintain.

## 10. What this kills in the current code

| Current | Fate |
|---|---|
| `agents.rs::KNOWN_CLIS`, `autonomous_flag()` | → catalogue data |
| `monitor/pty_listener.rs::extract_commands()` (`Bash(` scraping) | deleted — per-CLI rendering knowledge |
| `prompts.rs` PM prompt (relative paths, shell-out protocol) | → PM skill + MCP tools |
| PTY mirror chat pane + `intercept_subagent_launches` | → event-driven chat pane |
| `2026-08-15-pty-terminal-emulation-design.md` (vt100/tui-term) | superseded by this doc |
| `serde_yaml` (archived upstream) | → `toml` |
| `subagent-lock.yaml` (no pid) | → JSONL with pid (enables `mana ps`/`kill`) |

Known v1 defects fixed structurally: prompt-vs-code path mismatch, double
launch, `$PATH` dependency, lock-file read-modify-write race (single writer:
mana), unbounded chat buffer, PM-death never detected.

## 11. Verified vs. open

**Verified this session (on this machine):** claude bidirectional stream-json
multi-turn; ACP flags on copilot/gemini/opencode; no ACP/MCP on agy; `agy
models` / `opencode models` runtime discovery; MCP flags on 4/5 CLIs;
quota introspection absent everywhere; SKILL.md read from `~/.agents/skills`
by opencode (and the dir is real, 365 skills); PM-turn cost ≈ $0.11–0.53 with
default tools (the earlier $1.57 anomaly was `--tools ""` inflating context to
~167k tokens and zeroing cache reads — never pass it).

**Open / to verify before or during implementation:**
- ACP conformance per CLI (esp. `mcpServers` in `session/new`, permission flow).
- `oneshot-continue` semantics on agy: does `--continue` preserve context
  across print-mode calls in practice; cost profile per turn.
- Sentinel reliability (malformed blocks → validation + one retry message).
- `[pm.events]` JSONPaths against each CLI's real stream (golden transcripts).
- Windows end-to-end (spawn, worktrees, long paths) — CI covers unit level.
- `copilot monitoring` / `opencode export` as post-hoc enrichment channels
  (untested; candidate catalogue fields, not in the design).
- Role texts drafted in `assets/roles/` (`pm/SKILL.md`, `executor.md`,
  `reviewer.md`) — to be embedded via `include_str!` and battle-tested with the
  skill-creator eval loop before v2 ships. The reviewer writes its verdict as
  JSON at a mana-given path (`verdict`, `attribution: code|brief`, `issues[]`),
  which is what `get_review` parses.

## 12. Roadmap fit

Unchanged from the existing roadmap: `mana doctor` (now also reports catalogue
age and per-CLI capability degradations), `mana update <cli>`, `mana ps`/`kill`
(unblocked by the pid field), `mana upgrade` (release-plz + cargo-dist replace
the hand-written release workflow; same `self_update` client path), `--help`
discipline, lazy `~/.mana` creation.
