# Contributing to mana

Thanks for looking. This file is short on purpose — the details live next to
the work, and an agent you send at this repository can read them itself.

## The one thing that is easy to get wrong

**Open pull requests against `develop`, not `main`.**

`main` is the release branch. It moves only when a release is cut, so between
releases it lags behind and does not carry the current tree. A PR against
`main` looks plausible and cannot be merged cleanly. `gh pr create` defaults to
`main` and will not warn you, so pass `--base develop` explicitly.

```sh
git checkout -b fix/short-slug origin/develop
```

## Before you push

The same three commands CI runs, on Linux, macOS and Windows:

```sh
cargo fmt --all
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

Minimum supported Rust is **1.88** (set by `rmcp`, not by edition 2024's own
1.85). Clippy runs at `-D warnings`, so a lint fails the build. `--locked` is
deliberate: it proves the committed `Cargo.lock` builds, not merely that some
resolution does.

## Commits and pull requests

Commit subjects are `<type>: <what changed>` — `feat`, `fix`, `docs`, `test`,
`refactor`, `chore` — describing the behaviour rather than the diff. Put the
reasoning in the body; the diff already says what changed.

The pull-request template is `.github/PULL_REQUEST_TEMPLATE.md`. GitHub fills
it in for you in the browser. Its sections are not bureaucracy: each one
answers a question a reviewer would otherwise have to ask a day later. The two
that carry the most weight are **why this fix rather than a smaller one**, and
**what you could not verify** — both are invisible from the diff, and both save
a round trip.

## If you are working with a coding agent

You do not have to explain any of the above to it. This repository ships the
conventions as skills, under `.claude/skills/`:

| Skill | What it covers |
|---|---|
| `mana-open-pr` | branch to cut, branch to target, the checks, commit wording, the exact `gh` command |
| `mana-pr-description` | how to fill the PR template so the change is reviewable |

Point your agent at the repository and ask it to open a pull request; it will
find them. If it targets `main` anyway, that is a bug in those skills worth
reporting.

## Adding support for another agent CLI

mana can only drive CLIs the catalogue knows about — a name alone has no spawn
flags, no failure signatures and no PM driver. Add an entry to
`~/.mana/catalog.local.toml` to try one locally; if it works, a PR moving that
entry into `catalog/` is very welcome. Say in the description how you tested it
and against which version, since nobody else can reproduce it without the same
CLI installed.

## Reporting a bug

The most useful bug report says what you ran, what happened, and what you
expected — in that order. If mana told you something that was not true, quote
it verbatim: the wording is usually the bug.
