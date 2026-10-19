# Contributing to AoE

Search existing [issues](../../issues) and [pull requests](../../pulls) before
starting. Discuss significant features or architecture changes in an issue.
All contributions follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Contribution scope

AoE favors a simple, focused core. Explain why a proposed feature belongs there
and how its user benefit justifies added interface, configuration, and maintenance
complexity. A smaller diff alone does not make a feature a better fit.

Project fit involves maintainer taste and judgment, not just a checklist.
Explain how the feature would work, how it fits existing workflows, and its
tradeoffs so maintainers can evaluate the design. Discussion can change an
initial view that a feature does not belong in core. These guidelines help that
conversation; meeting them does not guarantee acceptance.

- Explain the user problem and why the existing workflow does not solve it.
  Check assignees and active PRs before starting overlapping work.
- Discuss significant features or architecture changes before implementation.
  Work toward a shared understanding of the design, acceptance criteria, and
  non-goals. An issue is not necessarily an accepted design; follow the latest
  agreed scope, or explain new reasoning when asking to revisit it.
- Fix the responsible layer without overriding user configuration or changing
  unrelated workflows. If approval calls for opt-in behavior, keep it opt-in.
- Prefer existing sources of truth over parallel schemas, settings, or modes.
  Explain any new dependencies and which build paths need them.
- Keep the change focused on the accepted problem; propose unrelated refactors
  or improvements separately. Provide reproducible evidence and show the changed
  behavior working, following the testing guidance in [AGENTS.md](AGENTS.md).

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

AI-assisted contributions follow the same rules. Agents make recommendations and
proposals, not project decisions: a human stays in the loop to approve scope and
implementation, review proposed changes, and decide issue disposition or merge.
Disclose tools and use in the [PR template](.github/pull_request_template.md);
you remain responsible for the change. Answer reviewer questions yourself rather
than pasting generated replies.

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
