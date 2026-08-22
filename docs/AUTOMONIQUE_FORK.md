# Automonique fork policy

This repository is the maintained Automonique fork of
[`1jehuang/jcode`](https://github.com/1jehuang/jcode). The fork preserves
JCode's standalone product while carrying the narrowly separated backend and
headless-engine work needed by Automonique.

## Recorded baseline

- Upstream remote: `https://github.com/1jehuang/jcode.git`
- Upstream branch: `master`
- Fork remote: `https://github.com/benfavre/jcode.git`
- Fork branch: `master`
- Synchronized upstream commit: `6da6124a30b0157dc9981c2fac5045f9590a6a05`
- Synchronization method: fast-forward, with no rewritten published history

The baseline commit remains an upstream commit. Automonique-specific changes
start after it and land through ordinary pull requests, so `git log` and
`git merge-base` can distinguish inherited code from fork patches without a
separate claim database.

## Synchronization procedure

Synchronize before an Automonique integration release, before beginning a
large backend change, and at least weekly while the integration is active.

```sh
git remote get-url upstream
git fetch --prune upstream master
git fetch --prune origin master
git rev-list --left-right --count origin/master...upstream/master
git switch master
git merge --ff-only upstream/master
scripts/check_guardrails.sh
git push origin master
```

The `--ff-only` step is the normal path and must stop if fork commits have been
placed directly on `master`. In that case, create a synchronization branch,
merge `upstream/master` there without rewriting either side, resolve conflicts
explicitly, run the complete guardrails, and merge that branch by pull request.
Never force-push either published branch.

Record the upstream SHA and divergence counts in the synchronization pull
request or tracking issue. A successful fetch or an online remote is not proof
that the fork builds; the guardrails are the build/test evidence.

## Divergence policy

Changes that improve JCode independently of Automonique should be proposed
upstream first or promptly after landing here. Examples include provider fixes,
TUI correctness, protocol hardening, accessibility, performance, and generic
backend seams.

Fork-only changes are limited to Automonique branding/launch integration,
Automonique platform transport adapters, authority and receipt mapping, and
packaging that produces `automonique tui`. Those changes must remain behind a
backend boundary: standalone mode may not require an Automonique daemon,
credential, schema, or build dependency.

The fork does not carry opportunistic product changes unrelated to either the
integration or an upstreamable fix. That keeps conflict resolution reviewable
and makes removal of a fork patch possible.

## CI and release policy

Every pull request runs JCode's normal standalone guardrails. Once the managed
backend feature exists, CI also builds and tests that feature explicitly; a
default/standalone build must continue to succeed with it disabled. Shared
backend behavior belongs in conformance tests exercised by both modes rather
than in two independently maintained test suites.

Fork releases use their own version/tag and identify both the fork commit and
the upstream merge base. No upstream tag is moved or reused for a fork build.
The Automonique repository pins the exact JCode artifact/source digest it
adapts; deployment never resolves a floating branch.

## Licence and provenance

The inherited repository remains MIT licensed and retains the upstream
copyright and licence notice. Directly adapted upstream files keep applicable
notices. Code copied or structurally adapted into another repository must be
listed in that repository's third-party provenance inventory before
distribution, with the exact fork commit and source path.

Automonique-specific commits should be small enough that `git diff` against the
recorded upstream merge base is a useful provenance view. Generated outputs are
regenerated from their source and are not used to obscure an upstream or local
change.
