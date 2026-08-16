# mana v3 — the PM writes, mana runs

**Status:** Approved direction (Ahmet, 2026-08-16) — supersedes nothing; builds
on `2026-08-15-mana-v2-design.md`, which stays the record of the shipped v2.

v2 proved the thesis: one PM plans on expensive quota while sub-agents execute
on cheap, otherwise-idle pools, on three drivers and four CLIs, with every
per-CLI fact living in the catalogue. v3 makes it a product. Its one-line
summary: **strip the agents down to judgment, and let Rust do everything that
is a rule.**

## 1. Doctrine

Two pillars, stated by Ahmet and adopted as the measure of every v3 decision:

1. **Judgment to agents, mechanics to code.** Anything expressible as a rule
   is enforced by mana, not requested of a model. Every mechanical PM turn
   removed is money saved and drift prevented — v2 measured a PM turn at
   $0.11–0.53, and its skill could only *ask* the model to follow the
   escalation policy.
2. **Least privilege per role.** Each agent gets exactly the tools its task
   needs, expressed per-CLI as catalogue data. A reviewer that cannot edit
   cannot cheat; an executor that cannot leave its worktree cannot damage.

The v2 rule is inherited unchanged: anything that differs between CLIs is a
catalogue field, never a code branch.

## 2. The scheduler

v2's loop asked the PM to dispatch executors, wait for notifications, launch
reviewers, read verdicts and re-dispatch — none of which requires judgment.
v3 moves the whole loop into mana as a per-task state machine:

```
pending ──► blocked(deps) ──► queued ──► running(executor)
                                            │ exit 0
                                            ▼
                              reviewing(reviewer) ──► validated ✓
                                            │ rejected(code) ×N
                                            ▼
                        retried (new task, issues appended) ──► …
                                            │ N = 3
                                            ▼
                                    escalated → needs_pm
```

Rules owned by code, no longer by prose:

- **`depends_on` is enforced**: a task is `blocked` until every dependency is
  `validated`. (v2 recorded the field and gated nothing — the honest gap the
  4.5 sweep exposed.)
- **The reviewer is automatic** after a clean executor exit. A dirty exit
  matching a quota signature cools the pair and re-queues the task on the
  next-cheapest candidate; a dirty exit with no signature counts as a failed
  attempt.
- **Escalation is deterministic**: three code-attributed rejections of the
  same piece of work escalate the cost class once and notify the PM as
  information. A brief-attributed rejection routes to the PM as a decision
  (`needs_pm`): only the PM can rewrite a brief.
- **Concurrency, cooldowns and `max_concurrent`** all keep their v2 homes;
  the scheduler is their only caller.

The PM's tool surface shrinks to three: `create_task`, `get_review`,
`list_agents`. `launch_subagent` is deleted — not deprecated, deleted; a
human override is a CLI command (`mana run <task> --cli X`), never a PM tool.
The task file grows optional routing data instead: `cost_class` and
`cli`/`model` hints in the frontmatter, written at planning time.

The PM is re-entered only at judgment points: a brief-attributed rejection,
an escalation exhausted, all tasks settled, or the user speaking. Each
re-entry carries the relevant state, so the PM never polls and never tracks.

## 3. Least privilege per role

The catalogue's `[pm].permission_args` generalizes to per-role privilege
argv, one table per role, resolved by the same substitution machinery:

| Role | Needs | claude (measured mechanism) | agy | opencode |
|---|---|---|---|---|
| pm | read + mana tools | `--allowedTools mcp__mana__*,Read,Grep,Glob` (verified live) | print mode denies all tools (verified) | none — skill text only (documented degradation) |
| executor | read/write/bash in worktree | allowlist without web/MCP extras | `--dangerously-skip-permissions` (as today) | `--dangerously-skip-permissions` |
| reviewer | read + run tests, **no edit** | allowlist minus Edit/Write | `--mode plan` (candidate, to measure) | none — documented |

Two derived moves complete the doctrine, both removing a privilege by moving
the mechanics into mana:

- **mana writes the verdict.** The reviewer prints its JSON verdict to
  stdout; mana captures, validates and writes `reviews/<task>.json` itself.
  The reviewer loses its last write need. (The malformed-verdict corrective
  retry moves into the scheduler at the same time — v2 shipped it only in the
  dev path.)
- **mana makes the commit.** The executor edits files; mana stages and
  commits `mana:{task_id}` in the worktree when the run ends cleanly. The
  executor loses git entirely, and every commit gains a uniform author,
  message and moment.

## 4. One identity, and the receipt

**agent_id unification.** v2 minted a PM-facing id in the MCP layer and a
registry id in the dispatcher; nothing could join them (found when `mana ps`
had to derive `done` from the exit log). v3 mints one id at task-dispatch
time — the scheduler's — and every writer (registry, logs, notifications,
runs) carries it.

**`mana stats` — the thesis made visible.** The counters already exist per
(CLI × model); v3 adds the reading: a `mana stats` command and a session
line in the TUI status bar summarizing what ran where and what the expensive
pool was spared. The number shown is honest by construction: tokens actually
consumed per pool as the CLIs reported them, and the cost avoided is labeled
an estimate. This is the screenshot people share; it is also the everyday
proof the routing works.

**`mana replay <task>`.** Dispatch stdout/stderr already lands in capped
files; replay renders a task's full story — brief, attempts, verdicts,
timings — in the TUI or plain text. Post-mortems become a gesture, and golden
transcripts get a natural source.

## 5. mana × hone

Decision recorded 2026-08-15: hone integrates into mana as a product bet —
every mana install ships command-output compression for its sub-agents. The
two measured weaknesses cancel: hone's filter-writing conversion (15% when it
begs a busy agent) meets mana's dispatcher; a missing filter becomes a mana
task (cheapest model writes it, the reviewer validates it against hone's own
rule) instead of an interruption.

Three gates, in order, each blocking the next:

- **A — the engine guardrail ships in hone first**: a filter that answers
  empty on a failed command is treated as having declined; property-tested
  across every shipped filter. Without A, bundling hone would let mana's own
  review loop be lied to. (Dispatched to an external agent 2026-08-16 —
  the first live run of the delegation protocol.)
- **B — bundled, consented, visible**: cargo-dist ships both binaries;
  `mana install` asks before wiring hone per CLI; the attach mechanism is
  catalogue data (per-invocation config injection — claude's `--settings`,
  plugin dirs elsewhere; never a silent edit of user configs); `mana doctor`
  reports hone's status; savings feed `mana stats`.
- **C — the moat**: hone's self-dispatch requests route through mana's
  scheduler as ordinary tasks. Filters become reviewed artifacts; the
  combined product learns per-project with a built-in review pipeline —
  which no static-catalogue competitor (RTK, chop, tokf) has.

Licenses for both crates are a precondition of B and remain Ahmet's call.

## 6. Trust, in stages

The worktree protects the repository; nothing yet protects the system. v3
answers in escalating stages, stopping where the evidence says it is enough:

1. **Per-CLI native sandboxes as catalogue data** — `[roles.*]` gains
   `sandbox_args` where a CLI offers a mechanism (agy ships `--sandbox`;
   claude's sandboxed bash; others documented as absent). No code branch.
2. **`TRUST.md`** — the threat model stated plainly: what the worktree
   guarantees, what each CLI's flags add, what remains open (network, HOME).
   An honest page beats an implied promise.
3. **OS-level confinement** (Landlock / `sandbox-exec` / AppContainer) —
   only if 1+2 prove insufficient for public adoption; costed then, not now.

## 7. Windows, for real

v2 compiles and unit-tests on Windows; the process story is honest but
degraded (no group kill, unverifiable pid guard). v3 brings parity where the
OS allows: Job Objects for spawn-and-kill of whole trees, a pid guard built
on process creation time, and one end-to-end smoke on the Windows runner
(scripted PM, as the unix M2 smoke). "Tout terrain" becomes a tested claim.

## 8. PM portability

Because every durable fact lives on disk — tasks, verdicts, registry,
counters, notifications — the PM conversation is expendable. v3 makes that a
feature: launch-time activation includes a compact state recap (open tasks by
state, last verdicts, cooling pairs), so `mana launch <other-cli>` after a
quota death resumes the project with a different brain. The fallback story
closes: sub-agents reroute around a dead pool (v2), and now the PM does too.

## 9. Kill list

- `launch_subagent` (MCP tool, skill text, tests) — replaced by the scheduler.
- The PM-facing agent_id mint in `mcp.rs` — replaced by the scheduler's.
- The skill's dispatch-loop instructions — replaced by re-entry messages;
  the skill shrinks to planning, brief-writing and verdict judgment.
- `dispatch_reviewer(..., None)`'s no-retry path — the corrective retry
  becomes scheduler-owned.
- The status bar's `no usage reported yet` filler — `mana stats` owns usage.

## 10. Verified vs open

Verified already (v2 evidence): per-role enforcement mechanisms on claude and
agy; opencode's absence of one; turn boundaries per driver (goldens); the
counters; worktree isolation under parallel dispatch; quota signatures for
claude and copilot.

Open, each with its planned check:

- agy `--mode plan` as reviewer confinement — one measured session.
- Scheduler re-entry pacing — does a re-entered PM stay coherent across many
  short turns on all three drivers? (Live milestone per phase, as in v2.)
- Ollama-backed opencode models (`ollama … :cloud`) — daemon and provider
  unconfigured on the reference machine; measure before cataloguing.
- hone gate A quality — under external-agent delivery, subject to the
  intransigent-review protocol.
- Job Objects behavior under the CLIs' own process trees.

## 11. Phasing sketch (plan document to follow)

- **P1 — the scheduler + least privilege.** Milestone: a multi-task project
  with dependencies runs to completion on 3 CLIs with the PM speaking only at
  plan and judgment points; reviewer provably unable to edit.
- **P2 — identity, stats, replay.** Milestone: one session's receipt shows
  real per-pool numbers; `mana replay` reconstructs a failed task end to end.
- **P3 — hone A/B.** Milestone: a fresh-machine install offers hone, a
  sub-agent's `cargo test` output arrives filtered, and the receipt shows it.
- **P4 — trust + Windows + portability.** Milestone: the Windows runner
  passes an end-to-end smoke; killing the PM's CLI mid-project and relaunching
  on another CLI resumes cleanly.
