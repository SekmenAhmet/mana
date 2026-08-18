# CI Pipeline — Design

**Date:** 2026-08-14
**Status:** Approved

## Purpose

`mana` has no automated checks today: no CI, single `main` branch, no lint/test
gate on push or PR. This adds a GitHub Actions CI pipeline that runs build,
test, and lint on every push to `main` and every pull request targeting
`main`. Scope is CI only — no release/CD automation (project is still v0.1.0,
not yet published anywhere).

## Design

### Workflow: `.github/workflows/ci.yml`

**Triggers:** `push` to `main`, `pull_request` targeting `main`.

**Matrix:** `ubuntu-latest`, `macos-latest`. Windows is out of scope — `mana`
isn't targeting it, and the PTY/terminal behavior (`portable-pty`,
`crossterm`) is exactly the kind of thing that differs across Linux/macOS, so
both are worth covering; a third OS with unclear support isn't.

> **Amendment (2026-08-18, #188):** Windows joined the matrix
> (`.github/workflows/ci.yml`) once v2 claimed cross-platform support —
> claimed-but-untested is how the v1 flow shipped broken. The reasoning above
> still explains why Linux and macOS were the first two covered; it was never
> meant as a permanent boundary, and the mana-upgrade design's "out of scope"
> bullet that cited this decision has been amended to match.

**Single job**, steps in order (fail fast on style before paying for a full
build):

1. `actions/checkout@v4`
2. `dtolnay/rust-toolchain@stable` with `rustfmt` + `clippy` components
3. `Swatinem/rust-cache@v2` — caches `~/.cargo` and `target/` keyed on
   `Cargo.lock`
4. `cargo fmt --all -- --check`
5. `cargo clippy --all-targets --all-features -- -D warnings`
6. `cargo build --all-targets --verbose`
7. `cargo test --all-targets --verbose`

### README

Add a CI status badge near the top of `README.md`, pointing at the
`ci.yml` workflow badge URL for `SekmenAhmet/mana`.

### Git workflow for this change

New branch `ci/github-actions-pipeline` off `main`. Commit the workflow file
and README badge. Push, open a PR against `main`. Wait for the new CI to run
and pass on its own PR. Ask Ahmet for explicit go-ahead before merging — push
and merge are never done without that confirmation, regardless of the
original request to "handle the merge."

## Out of scope

- Release/CD automation (binary builds, GitHub Releases, crates.io publish)
- Branch protection rules on `main`
- Auto-merge for future PRs

These were explicitly declined for this pass and can be a follow-up if
wanted later.

## Testing

The pipeline is validated by itself: the PR that introduces it must show the
`ci` job passing on both `ubuntu-latest` and `macos-latest` before merge.
