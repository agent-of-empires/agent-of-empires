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
- Keep tests deterministic and isolated from user state.
- Use conventional commit and PR titles, for example
  `fix: preserve sessions after reconnect`.

`feat`, `fix`, `perf`, `security`, and `revert` changes appear in generated
release notes. Maintenance types such as `build`, `chore`, `ci`, `docs`,
`refactor`, `style`, and `test` do not. A web scope follows the same rule, so
`feat(web)` is visible and `refactor(web)` is not. The PR title check enforces
`<type>(<scope>)?: <lowercase subject>`.

## Pull requests

Open the PR against `main` and complete the template. Include:

- what changed and why;
- how it was tested;
- screenshots or recordings for UI changes;
- related issues.

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

Add `ci-macos` when a PR needs macOS coverage; a skipped check is not coverage.
The [labeler](.github/workflows/labeler.yml) requests it conservatively for
sensitive changes or potentially incomplete file data. When adding macOS-specific
code or tests outside its covered paths, extend its sensitive-file list. It runs
from the base branch, so new or changed detection rules may need a manual label.

Releases are staged weekly. Maintainer instructions live in
[docs/development/releases.md](docs/development/releases.md).

Merge rules and area owners are in [MAINTAINERS.md](MAINTAINERS.md).

## Review rubric

Reviewers tag each requested change with a priority:

- **P1**: breaks behavior, security, or stored data. Blocks merge.
- **P2**: a correctness, test, or maintainability gap this PR should close.
  Blocks merge.
- **P3**: a worthwhile improvement outside this PR. Becomes a follow-up issue,
  not a blocker.

Every requested change states what would satisfy it.
