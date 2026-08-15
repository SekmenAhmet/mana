# mana upgrade — Design

**Date:** 2026-08-15
**Status:** Approved

## Purpose

`mana upgrade` is documented (`Managent/commands/mana upgrade.md`) as "Met à
jour `mana` en récupérant et installant la dernière release du projet", but
today it's a stub that prints "pas encore disponible en v1" and does
nothing. This gives it a real implementation: a GitHub Actions release
workflow that publishes platform binaries on a tag push, and client logic in
`mana upgrade` that fetches and installs the latest one.

## Design

### Part 1 — Release workflow: `.github/workflows/release.yml`

**Trigger:** `push` on tags matching `v*.*.*`, pushed manually (`git tag
v0.2.0 && git push --tags`). No automation on merges to `main` — releases
stay a deliberate, versioned decision, not a side effect of every PR.

**Matrix**, one job per target, mirroring `ci.yml`'s OS coverage (Windows
stays out of scope, same rationale as the CI pipeline):

| OS runner | Target triple | Build |
|---|---|---|
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | native |
| `macos-latest` | `aarch64-apple-darwin` | native |
| `macos-latest` | `x86_64-apple-darwin` | cross (`rustup target add` + `--target`) |

Each job:
1. `actions/checkout@v4`, `dtolnay/rust-toolchain@stable` (+ target for the
   cross job), `Swatinem/rust-cache@v2`
2. `cargo build --release --target <triple>`
3. Package the binary as `mana-<triple>.tar.gz`
4. `gh release create "$TAG" --generate-notes` (only on the first job to
   reach this step — subsequent jobs skip creation if it already exists) then
   `gh release upload "$TAG" mana-<triple>.tar.gz`, using the built-in
   `GITHUB_TOKEN` — same `gh` CLI already relied on elsewhere in this
   project, no third-party marketplace action needed.

### Part 2 — Client: `src/cli/upgrade.rs`

New dependency: `self_update = "0.41"` (mature, handles the cross-platform
"replace the currently-running executable" dance — including the
Windows-specific rename-then-delete sequence — so `mana` doesn't reinvent
it).

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

`describe_update_result(status: &self_update::Status) -> String` is a pure
function covering both `Status` variants (`UpToDate`, `Updated`) — extracted
so the messaging logic has a unit test independent of any real network call.

### Error handling

`self_update`'s `update()` returns `Result`, propagated via `?` into the
existing `anyhow::Result<()>` chain `main.rs` already handles for every other
command (print the error, exit non-zero). No special-casing: "no releases
published yet", "no internet", and "rate-limited by GitHub" all surface as
the same kind of error the user already sees for e.g. a missing agent
binary.

## Out of scope

- Windows binaries/releases (matches the existing CI matrix's own scope
  decision)
- Automatic/scheduled releases — always a manually pushed tag
- Checksum/signature verification of downloaded binaries (self_update
  doesn't do this by default either; a v2 concern, not v1)

## Testing

- `describe_update_result` — unit tested for both `Status::UpToDate` and
  `Status::Updated` variants.
- The real `self_update().update()` call — a genuine network + self-replace
  boundary — is **not** unit tested, by the same established project
  convention that excludes `native_pty_system()`, `enable_raw_mode()`, and
  real `$HOME`/terminal resolution from coverage.
- The release workflow is validated the same way `ci.yml` was: by actually
  pushing a tag once implemented and confirming the release gets created
  with all three assets attached.
