---
name: mana-pr-description
description: >-
  How to fill in mana's pull-request template so a reviewer can actually judge
  the change: what belongs in each section, what evidence counts as
  verification, and how to describe a ceiling instead of hiding it. Use this
  skill whenever you are writing or rewriting a PR description, a PR body, or a
  change summary for the mana repository — including when someone says "write
  the PR", "describe this change", "fill in the template", or asks you to
  improve a description that already exists. Pair it with mana-open-pr, which
  covers the branch, the checks and the `gh` command.
---

# Writing a mana pull request

The template lives at `.github/PULL_REQUEST_TEMPLATE.md`. GitHub pre-fills it
in the browser; if you are creating the PR with `gh`, read that file and fill
it in yourself rather than inventing your own headings — a reviewer reading
twenty PRs benefits from them all being shaped the same way.

What follows is what each section is *for*. The headings are cheap; the reason
they exist is not.

## Aim the description at the reviewer's questions

A reviewer can read the diff. What they cannot read is everything you decided
not to write down. Every section of the template exists to capture one thing a
reviewer would otherwise have to ask for in a comment, a day later.

So the test for a good description is not "is it thorough" but: **after reading
this, does the reviewer still have to ask me something before they can approve
it?**

## What this changes

State the behaviour difference from the outside. "A corrupt task file no longer
reads as a missing one" is a sentence a reviewer can check against the diff.
"Changed the error branch in `read_task`" is the diff restated, which helps
nobody.

If the change is user-visible — a command, a message, an exit code — quote the
before and after. A one-line diff of the actual output is worth a paragraph of
description.

## Why

This is the section that carries the most weight, and the one most often
skipped.

Say what was wrong. Then say why *this* fix rather than the smaller or more
obvious one. If you tried something first and it did not work, that belongs
here — it stops the reviewer suggesting the thing you already ruled out, and it
stops the next person retrying it in six months.

Two shapes that are especially worth writing out:

- **The fix is bigger than the report.** If you fixed the shared function
  rather than the one call site the issue named, say so and say which other
  callers were affected. It looks like scope creep until you explain it, and it
  is usually the right call.
- **The obvious fix was wrong.** If the issue itself proposed a fix and you did
  something else, say why. A prescription inside a ticket is a hypothesis, not
  a specification, and a reviewer who trusts the ticket will wonder why you
  deviated.

## How it was verified

Paste what you ran and what it produced. "Tests pass" is not evidence;
`571/571` is.

The distinction that matters for a bug fix: a test that passes after your
change proves nothing on its own — it may have passed before. What proves the
fix is a test that **fails without your change and passes with it**. If you
checked that, say so explicitly, because it is the single most convincing line
in a PR and it is invisible from the diff.

For anything concurrent, timing-dependent, or platform-specific, describe the
reproduction rather than asserting the property. "Eight threads, four hundred
appends each, every line must parse; the old code produced a torn line" is
checkable. "Appends are now atomic" is a claim.

Be honest about the limits of what you ran:

- Windows and Linux behaviour you could not execute locally — say it is
  compile-checked only, and let CI be the real check.
- A dependency or agent CLI you do not have installed — say which path is
  therefore untested.

Saying what you could not verify costs you nothing and tells the reviewer
exactly where to look. Silence there reads as coverage you do not have.

## Anything left undone

Name the ceilings. A deliberate simplification, a case you decided not to
handle, a follow-up you think is worth its own issue.

The reason this matters: a limitation you write down is a decision the reviewer
gets to make with you. The same limitation left out is a bug report someone
files in three weeks, by which time nobody remembers it was on purpose.

Write it plainly — "the checksum is served from the same origin as the archive,
so this bounds a tampered transfer and not a tampered release; signing is the
upgrade path" — not as an apology.

"Nothing" is a perfectly good answer when it is true. Do not manufacture
caveats to look thorough.

## Length

Match the change. A one-line typo fix does not need five sections of prose —
fill in what applies, write "n/a" under the rest, and keep the headings so the
reviewer can see you considered them.

A large or surprising change earns a long description. If the explanation is
longer than the diff and the diff is three lines, something is off: either the
change is more subtle than it looks (say so — that is the description doing its
job) or the prose is padding.
