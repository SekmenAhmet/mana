<!--
Leave the headings in place and replace the italic prompts under them.
A section that genuinely does not apply: write "n/a" and why, rather than
deleting the heading — a reviewer can tell "considered and not applicable"
from "forgotten" only if the heading is still there.
-->

## What this changes

_One or two sentences. What behaviour is different after this merges? Write it
from the user's side, not the diff's — "a corrupt task file no longer reads as
a missing one" tells a reviewer more than "changed the error branch in
read_task"._

Closes #

## Why

_What was wrong, and why this is the fix rather than another one. If you
considered a smaller or more obvious approach and rejected it, say which and
why — that is usually the most useful paragraph in the whole description,
because it is the one a reviewer cannot reconstruct from the code._

## How it was verified

_What you ran, and what it proved. Paste the result, not the intention._

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] `cargo test --locked --all-targets`

_For a bug fix, the useful evidence is a test that **fails before your change
and passes after**. Say whether you checked that — a test that passes both ways
proves nothing, and it is an easy thing to ship by accident._

_Anything you could not verify on your machine (Windows behaviour, a real agent
CLI you do not have installed) belongs here too. CI covers some of it; saying
what you could not check is more useful than silence._

## Anything left undone

_Known limits, deliberate simplifications, follow-ups you chose not to do here.
A ceiling you name is a reviewer's decision to make. A ceiling you hide is a
bug report someone else files in three weeks._

_"Nothing" is a fine answer if it is true._
