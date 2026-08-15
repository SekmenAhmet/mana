# mana upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `mana upgrade` stub with a real implementation that fetches and installs the latest GitHub release, and add the release workflow that publishes the binaries it needs.

**Architecture:** Two independent pieces. (1) `.github/workflows/release.yml` builds `mana` for three targets on a `v*.*.*` tag push and uploads each as a `.tar.gz` asset on a GitHub Release. (2) `src/cli/upgrade.rs` uses the `self_update` crate to compare the running binary's version against the latest release and, if newer, download and replace it — with the version-comparison messaging split into a pure, unit-tested function and the real network/self-replace call left untested by design (same convention as `native_pty_system()`/`enable_raw_mode()` elsewhere in this codebase).

**Tech Stack:** Rust, `self_update` crate (GitHub Releases backend, tar/gzip archive support), GitHub Actions, `gh` CLI.

**Spec:** `docs/superpowers/specs/2026-08-15-mana-upgrade-design.md`

## Global Constraints

- Release matrix is `x86_64-unknown-linux-gnu` (ubuntu-latest), `aarch64-apple-darwin` (macos-latest, native), `x86_64-apple-darwin` (macos-latest, cross) — no Windows, matching `ci.yml`'s existing OS scope.
- Releases are only published on a manually pushed `v*.*.*` tag — never automatically on merge to `main`.
- No checksum/signature verification of downloaded binaries in this pass (explicitly out of scope per spec).
- Every task must leave the repo passing `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build --all-targets`, and `cargo test --all-targets` before it's committed.

---

### Task 1: `self_update` dependency + pure result-formatting function

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/cli/upgrade.rs`

**Interfaces:**
- Produces: `pub(crate) fn describe_update_result(status: &self_update::Status) -> String` — used by Task 2's `run()`.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[dependencies]`, add (alphabetical position after `serde_yaml`, before `strip-ansi-escapes` is fine — match existing style, one line):

```toml
self_update = { version = "0.41", features = ["archive-tar", "compression-flate2"] }
```

The `archive-tar`/`compression-flate2` features are required because the release assets built in Task 3 are `.tar.gz` — without them `self_update` can't extract the downloaded archive.

Run: `cargo build` — expect it to succeed (just resolves the new dependency, nothing uses it yet).

- [ ] **Step 2: Write the failing test**

Replace the entire contents of `src/cli/upgrade.rs` with:

```rust
pub(crate) fn describe_update_result(status: &self_update::Status) -> String {
    todo!()
}

pub fn run() -> anyhow::Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use self_update::Status;

    #[test]
    fn describe_update_result_reports_already_up_to_date() {
        let status = Status::UpToDate("0.1.0".to_string());
        assert_eq!(
            describe_update_result(&status),
            "mana est deja a jour (0.1.0)"
        );
    }

    #[test]
    fn describe_update_result_reports_new_version_installed() {
        let status = Status::Updated("0.2.0".to_string());
        assert_eq!(
            describe_update_result(&status),
            "mana mis a jour vers la version 0.2.0"
        );
    }
}
```

Run: `cargo test --all-targets describe_update_result`
Expected: FAIL (panics on `todo!()`) for both tests.

- [ ] **Step 3: Implement `describe_update_result`**

Replace the `todo!()` body:

```rust
pub(crate) fn describe_update_result(status: &self_update::Status) -> String {
    match status {
        self_update::Status::UpToDate(version) => format!("mana est deja a jour ({version})"),
        self_update::Status::Updated(version) => {
            format!("mana mis a jour vers la version {version}")
        }
    }
}
```

Leave `run()`'s `todo!()` as-is for now — Task 2 implements it.

- [ ] **Step 4: Run the tests**

Run: `cargo test --all-targets describe_update_result`
Expected: PASS (2 passed). `run()` is never called by these tests, so its `todo!()` doesn't trip.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/cli/upgrade.rs
git commit -m "feat: add self_update dependency, format upgrade result messages"
```

---

### Task 2: Wire the real upgrade logic into `run()`

**Files:**
- Modify: `src/cli/upgrade.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `describe_update_result` from Task 1.
- Produces: `pub fn run() -> anyhow::Result<()>` — `main.rs`'s `Command::Upgrade` arm now needs `?` since this no longer returns `()`.

- [ ] **Step 1: Implement `run()`**

In `src/cli/upgrade.rs`, replace the `run()` stub:

```rust
pub fn run() -> anyhow::Result<()> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner("SekmenAhmet")
        .repo_name("mana")
        .bin_name("mana")
        .target(self_update::get_target())
        .show_download_progress(true)
        .current_version(env!("CARGO_PKG_VERSION"))
        .build()?
        .update()?;
    println!("{}", describe_update_result(&status));
    Ok(())
}
```

This is the one part of this feature that is not unit tested — it makes a
real GitHub API call and, if a newer version exists, replaces the running
executable. Same category as `native_pty_system()`/`enable_raw_mode()`
elsewhere in this codebase: a real OS/network boundary, excluded from
coverage by design, not by oversight.

- [ ] **Step 2: Update the call site in `main.rs`**

Find (via `grep -n "Command::Upgrade" src/main.rs`):

```rust
        Command::Upgrade => cli::upgrade::run(),
```

Replace with:

```rust
        Command::Upgrade => cli::upgrade::run()?,
```

- [ ] **Step 3: Build**

Run: `cargo build --all-targets`
Expected: builds cleanly. (No test exercises `run()` itself — see Step 1's note — so nothing to run beyond a successful compile here.)

- [ ] **Step 4: Full verification**

Run in order:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
Expected: all three pass (fmt clean, zero clippy warnings, all tests green — including the two from Task 1, unaffected by this change).

- [ ] **Step 5: Commit**

```bash
git add src/cli/upgrade.rs src/main.rs
git commit -m "feat: mana upgrade fetches and installs the latest GitHub release"
```

---

### Task 3: Release workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- None — this is a standalone CI workflow, not exercised by `cargo test`.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/release.yml`:

```yaml
name: Release

on:
  push:
    tags:
      - 'v*.*.*'

env:
  CARGO_TERM_COLOR: always

permissions:
  contents: write

jobs:
  create-release:
    name: create release
    runs-on: ubuntu-latest
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Create GitHub release
        env:
          GH_TOKEN: ${{ github.token }}
        run: gh release create "${{ github.ref_name }}" --generate-notes

  build:
    name: build (${{ matrix.target }})
    needs: create-release
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - os: macos-latest
            target: aarch64-apple-darwin
          - os: macos-latest
            target: x86_64-apple-darwin
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - name: Cache cargo/target
        uses: Swatinem/rust-cache@v2

      - name: Build release binary
        run: cargo build --release --target ${{ matrix.target }}

      - name: Package binary
        run: |
          cd target/${{ matrix.target }}/release
          tar -czf "mana-${{ matrix.target }}.tar.gz" mana
          mv "mana-${{ matrix.target }}.tar.gz" "$GITHUB_WORKSPACE/"

      - name: Upload release asset
        env:
          GH_TOKEN: ${{ github.token }}
        run: gh release upload "${{ github.ref_name }}" "mana-${{ matrix.target }}.tar.gz"
```

`create-release` runs once, alone, before the matrix (`needs: create-release`)
so the three build jobs — which run in parallel — never race to create the
same release; they only ever upload to one that already exists.

- [ ] **Step 2: Validate YAML syntax locally**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" && echo "valid YAML"`
Expected: `valid YAML` (this only checks the file parses — the workflow
itself is validated for real the first time a `v*.*.*` tag is pushed, per
the spec's Testing section).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add release workflow, published on v*.*.* tags"
```

---

## Self-Review Notes

- **Spec coverage:** Part 1 (release workflow) → Task 3. Part 2 (client) →
  Tasks 1–2. Error handling section → Task 2 Step 1 (`?` propagation, no
  special-casing). Testing section → Task 1 (pure function tests) + Task 2
  Step 1's comment (real call excluded by design). Out-of-scope items
  (Windows, auto-releases, checksums) are simply absent from every task —
  nothing to point to, which is correct.
- **Placeholder scan:** The two `todo!()` in Task 1 Step 2 are intentional
  TDD scaffolding removed within the same task (Step 3), not a plan
  placeholder left dangling across tasks.
- **Type consistency:** `describe_update_result(status: &self_update::Status) -> String`
  is defined once in Task 1 and consumed with the same signature in Task 2
  Step 1 (`describe_update_result(&status)`).
