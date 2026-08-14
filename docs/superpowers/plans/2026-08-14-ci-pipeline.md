# CI Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a GitHub Actions CI workflow that runs fmt/clippy/build/test on every push to `main` and every PR targeting `main`, and a CI badge on the README.

**Architecture:** Single GitHub Actions workflow file (`.github/workflows/ci.yml`) with one job, matrixed over `ubuntu-latest` and `macos-latest`. No app code changes — this is infra/config only. Validation happens by pushing the branch and observing the Actions run on GitHub, since there's no local way to fully simulate the Actions runner.

**Tech Stack:** GitHub Actions, `dtolnay/rust-toolchain`, `Swatinem/rust-cache`, existing `cargo fmt`/`clippy`/`build`/`test`.

**Spec:** `docs/superpowers/specs/2026-08-14-ci-pipeline-design.md`

## Global Constraints

- Triggers: `push` to `main`, `pull_request` targeting `main` — no other triggers.
- Matrix: `ubuntu-latest` and `macos-latest` only — no Windows.
- Step order: fmt → clippy → build → test (fail fast on cheap checks first).
- Clippy must run with `-D warnings` (warnings are build failures).
- No release/CD, no branch protection, no auto-merge — out of scope for this plan.

---

### Task 1: CI workflow file

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: a GitHub Actions workflow named `CI` with a single job `ci`, runnable via GitHub's UI/API — no other task in this plan depends on its internals, Task 2 only depends on the workflow existing at this path (for the badge URL).

- [ ] **Step 1: Create the workflow file**

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always

jobs:
  ci:
    name: build, test, lint (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo/target
        uses: Swatinem/rust-cache@v2

      - name: Check formatting
        run: cargo fmt --all -- --check

      - name: Clippy
        run: cargo clippy --all-targets --all-features -- -D warnings

      - name: Build
        run: cargo build --all-targets --verbose

      - name: Test
        run: cargo test --all-targets --verbose
```

- [ ] **Step 2: Validate YAML syntax locally**

Run: `python3 -c "import yaml, sys; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK`

Expected: `OK` (no exception). This only catches YAML syntax errors, not
Actions-schema errors — the real validation is the Actions run after push
(Task 3).

- [ ] **Step 3: Sanity-check the commands locally**

Run each of these from the repo root and confirm they succeed (or note
pre-existing failures unrelated to this plan, e.g. formatting drift already
in the tree):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --verbose
cargo test --all-targets --verbose
```

If `cargo fmt --all -- --check` or clippy fail on pre-existing code (not
code this plan touches), fix that drift as part of this task — the whole
point of the pipeline is a clean baseline to gate future PRs on.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add GitHub Actions pipeline (fmt, clippy, build, test)"
```

---

### Task 2: README CI badge

**Files:**
- Modify: `README.md:1-3`

**Interfaces:**
- Consumes: the workflow name `CI` and path `.github/workflows/ci.yml` from Task 1, and the GitHub repo slug `SekmenAhmet/mana` (from `git remote -v`, already confirmed as `origin`).

- [ ] **Step 1: Add the badge under the H1**

Current `README.md` top:

```markdown
# mana

Orchestrateur d'agents IA de coding en CLI/TUI (Rust). Lance un agent CLI (Claude Code en v1) comme Project Manager, qui decoupe le travail en taches et delegue a des sous-agents executor/reviewer.
```

New top:

```markdown
# mana

[![CI](https://github.com/SekmenAhmet/mana/actions/workflows/ci.yml/badge.svg)](https://github.com/SekmenAhmet/mana/actions/workflows/ci.yml)

Orchestrateur d'agents IA de coding en CLI/TUI (Rust). Lance un agent CLI (Claude Code en v1) comme Project Manager, qui decoupe le travail en taches et delegue a des sous-agents executor/reviewer.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add CI status badge to README"
```

---

### Task 3: Push, open PR, verify, merge

**Files:** none (git/GitHub operations only)

**Interfaces:** none — terminal task, depends on Tasks 1-2 being committed on `ci/github-actions-pipeline`.

- [ ] **Step 1: Push the branch**

```bash
git push -u origin ci/github-actions-pipeline
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "ci: add GitHub Actions pipeline" --body "$(cat <<'EOF'
## Summary
- Add `.github/workflows/ci.yml`: fmt, clippy (-D warnings), build, test on push/PR to main, matrixed over ubuntu-latest/macos-latest
- Add CI status badge to README

## Test plan
- [ ] CI job passes on ubuntu-latest
- [ ] CI job passes on macos-latest
EOF
)"
```

- [ ] **Step 3: Watch the run and confirm both matrix legs pass**

```bash
gh pr checks --watch
```

Expected: both `ci (ubuntu-latest)` and `ci (macos-latest)` report success.
If either fails, fix the underlying issue (not the workflow) unless the
failure is in the workflow file itself, then push a fix commit and re-watch.

- [ ] **Step 4: Ask Ahmet for explicit go-ahead, then merge**

Do not merge without an explicit yes in this session, per project
constraints, even though the original request said to "handle the merge."
Once confirmed:

```bash
gh pr merge --squash --delete-branch
```
