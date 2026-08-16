---
name: mana-open-pr
description: >-
  How to land a change in the mana repository: which branch to cut, which
  branch to target, what to run before pushing, how to word the commit, and the
  exact `gh` invocation. Use this skill whenever you are about to commit, push,
  open a pull request, or "contribute" anything at all to mana — and read it
  BEFORE creating the branch, because the base branch is the one thing that
  cannot be fixed afterwards without reopening the PR. Also use it when someone
  asks you to "send a fix upstream", "open a PR for this", "submit this
  change", or when you have finished editing mana's source and need to get it
  reviewed.
---

# Landing a change in mana

The one thing to get right before anything else: **mana's integration branch is
`develop`, not `main`.**

`main` is the release branch. It only moves when a release is cut, so it lags a
whole release behind and does not carry the current tree — it is missing
config, tests and workflows that `develop` has. A pull request opened against
`main` will look plausible, may even go green, and cannot be merged without
creating a conflict for everyone. This has already happened to a first-time
contributor; it is the single most common way to waste your work here.

```sh
git fetch origin
git checkout -b <type>/<short-slug> origin/develop
```

`<type>` is the same word you will use in the commit subject: `feat`, `fix`,
`docs`, `test`, `refactor`, `chore`. The slug is a few kebab-case words about
the change, not the issue number — `fix/jsonl-torn-appends`, not `fix/70`.

## Before you push: run the same three commands CI runs

CI runs these on Linux, macOS and Windows. Running them locally first is not
ceremony — a red CI run costs a full round trip and, on a fork, it may not even
start until a maintainer approves the workflow, so you can be waiting on a
human for a failure you could have seen in ninety seconds.

```sh
cargo fmt --all
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

Three things about these that catch people out:

- `--locked` is deliberate. Without it cargo silently re-resolves dependencies
  and the green run proves that *some* dependency set builds, not the committed
  one. If `--locked` fails, your `Cargo.toml` and `Cargo.lock` disagree — commit
  the updated lock rather than dropping the flag.
- Clippy runs at `-D warnings`, so a lint is a build failure. If a lint is
  genuinely wrong for your case, prefer restructuring the code over
  `#[allow(...)]`; if you do add an allow, say in the PR why the lint is wrong
  here. `too_many_arguments`, for instance, usually means a struct is missing,
  not that the lint is noisy.
- The minimum supported Rust version is 1.88, set by `rmcp`. Edition 2024's own
  floor is 1.85, so "it compiles on my edition-2024 toolchain" is not enough.

Some tests spawn real processes and can time out if you run several `cargo`
invocations at once. A failure in `pm::stream`, `pm::oneshot` or `subprocess`
that disappears on a quiet re-run is contention, not your change — but re-run it
and say so, rather than assuming.

## Commit messages

The subject line is `<type>: <what changed>`, lowercase after the colon, in the
imperative or the plain present. Describe the behaviour, not the diff:

```
fix: stop telling the PM things that are not true
refactor: delete the ~/.mana/config.toml registry
test: decide the age guard without racing `ps`
```

The body is where the reasoning goes, wrapped at about 72 columns. Explain what
was wrong and why this is the fix — a reviewer can read the diff for the
"what", and cannot reconstruct the "why" from anything but your words.

Close issues from the body, one per line:

```
Closes #70
Closes #71
```

A caveat specific to this repo: those keywords only fire when the PR merges
into the **default** branch, and the default branch is `main`. Merging into
`develop` will not close anything, so a maintainer closes them by hand. Write
them anyway — they record the link.

## Opening the PR

```sh
git push -u origin <your-branch>
gh pr create --base develop --head <your-branch> --title "<type>: <subject>" --body "$(cat <<'EOF'
<the filled-in template>
EOF
)"
```

`--base develop` is not optional and `gh` will not warn you: it defaults to the
repository's default branch, which is `main`.

The body should follow `.github/PULL_REQUEST_TEMPLATE.md`. If you are writing
it yourself rather than filling the template in a browser, read the
**mana-pr-description** skill — a good description is most of what makes a
change reviewable, and the template's sections exist because each one answers a
question a reviewer would otherwise have to ask.

## After it is open

```sh
gh pr checks <number> --watch
```

Four checks must pass: `build, test, lint` on ubuntu, macOS and Windows, plus
`deps & coverage`. If one fails, read the failing step before changing
anything — `gh api repos/SekmenAhmet/mana/actions/jobs/<job-id>` names the step,
and the cause is often not the one the job title suggests. Coverage has a floor,
so deleting tests without deleting the code they covered will fail the build.

## What not to do

Do not merge your own PR, delete branches you did not create, force-push over a
reviewer's commits, or push directly to `develop` or `main`. If you have write
access, opening the PR and waiting is still the right move: CI runs on the PR,
not on your local machine.
