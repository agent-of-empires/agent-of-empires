# Contributing to AoE

Search existing [issues](../../issues) and [pull requests](../../pulls) before
starting. Discuss significant features or architecture changes in an issue.
All contributions follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Contribution scope

AoE favors a simple, focused core. Explain how a feature fits existing workflows
and justifies added interface, configuration, and maintenance complexity, not just
a small diff. Maintainers decide fit; discussion can change that judgment, but
these guidelines do not guarantee acceptance.

- Keep initial feature suggestions brief: explain the user problem, existing
  workflow's limits, and proposed change. Read and edit your submission yourself,
  including AI-generated text; respect maintainer reading time.
- Check assignees and active PRs to avoid duplicate work. Agree on significant
  designs, acceptance criteria, and non-goals before implementation; an issue
  alone is not approval. Follow the latest agreed scope or ask to revisit it.
- Fix the responsible layer, respect user configuration, and keep approved
  opt-in behavior opt-in. Reuse existing sources of truth; justify new dependencies
  and explain which build paths need them.
- Keep unrelated improvements separate. Provide reproducible evidence and show
  changed behavior working, following the [testing guidance](AGENTS.md).

## Setup

Install Rust, Git, and tmux. Node.js and npm are needed only for the web
dashboard.

```sh
git clone https://github.com/YOUR_USERNAME/agent-of-empires.git
cd agent-of-empires
git remote add upstream https://github.com/agent-of-empires/agent-of-empires.git
cargo build
cargo test
```

See [Development](docs/development.md) for local builds, logging, the web dev
server, and the optional shared build cache.

## Changes

- Use a `feature/`, `fix/`, `docs/`, or `refactor/` branch.
- Follow the coding and testing rules in [AGENTS.md](AGENTS.md).
- Format with `cargo fmt` and fix `cargo clippy` warnings.
- Update the documentation your change makes materially wrong or incomplete in
  the same PR: `docs/`, `CONTEXT.md`, ADRs, and generated references such as
  `docs/cli/reference.md`. Do not add docs for unchanged behavior; correcting
  documentation that is already wrong is in scope for any PR, docs-only ones
  included.
- Keep tests deterministic and isolated from user state.
- Use conventional commit and PR titles, for example
  `fix: preserve sessions after reconnect`.

`feat`, `fix`, `perf`, `security`, and `revert` changes appear in generated
release notes. Maintenance types such as `build`, `chore`, `ci`, `docs`,
`refactor`, `style`, and `test` do not. A web scope follows the same rule, so
`feat(web)` is visible and `refactor(web)` is not. The PR title check enforces
`<type>(<scope>)?: <lowercase subject>`. A `!` after the type or scope proposes
a major release; use it only for breaking changes that warrant one.

## Pull requests

Open the PR against `main` and complete the template. Include:

- what changed and why;
- how it was tested;
- related issues.

If it's a UI change, consider attaching screenshot(s), or certainly provide them if requested by the reviewer.

AI agents propose; humans approve scope and implementation, review changes, and
decide issue disposition or merge. You remain responsible for AI-assisted work:
disclose tools and use in the [PR template](.github/pull_request_template.md)
and answer reviewer questions yourself, not with generated replies.

Before requesting review, run:

```sh
cargo fmt --check
cargo clippy
cargo test
```

Web changes have additional checks in [web/AGENTS.md](web/AGENTS.md).

A skipped macOS check is not coverage. The `macOS gate` job in
[tests.yml](.github/workflows/tests.yml) runs macOS for sensitive changes or
potentially incomplete file data; when adding macOS-specific code or tests
outside its covered paths, extend its sensitive-file list. To force a run, add
`ci-macos` and re-run the `macOS gate` job.

Releases ship from a release-plz PR, with daily nightlies of `main`.
Maintainer instructions live in
[docs/development/releases.md](docs/development/releases.md).

Merge rules and area owners are in [MAINTAINERS.md](MAINTAINERS.md).

## Review rubric

Reviewers tag each requested change with a priority:

- **P1**: breaks behavior, security, or stored data. Blocks merge.
- **P2**: a correctness, test, or maintainability gap this PR should close.
  Blocks merge if it risks security or data loss, mistakes a migration or
  stored data, regresses user-visible behavior, or is one of three or more P2s
  in the review. Otherwise the reviewer approves and lists it as non-blocking.
- **P3**: a nit or optional improvement. Never blocks merge and needs no
  follow-up issue; mention it only when useful.

Every requested change states what would satisfy it. Documentation a PR leaves
materially stale counts as a finding: give it the priority its impact warrants
and name the document that must change. Do not ask for docs describing behavior
the PR does not change; correcting documentation that is already wrong is in
scope for any PR.

When the findings are few, none blocks under this rubric, and each fix is small,
the reviewer may push the fixes to the PR branch instead of waiting on the
contributor. Push a normal commit on top of the branch and say in the review
what was pushed. If the branch is behind `main` or conflicts, merge `main` into
it with a merge commit. Never rebase or force-push. Pushing needs the PR's
"Allow edits by maintainers" setting, which organisation-owned forks lack. For
those forks, and for contributors on the [exclusion
list](MAINTAINERS.md#reviewer-push-exclusions), leave the fixes to the
contributor and apply the rubric as usual.
