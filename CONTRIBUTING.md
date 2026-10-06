# Contributing to AoE

Search existing [issues](../../issues) and [pull requests](../../pulls) before
starting. Discuss significant features or architecture changes in an issue.
All contributions follow the [Code of Conduct](CODE_OF_CONDUCT.md).

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

Before requesting review, run:

```sh
cargo fmt --check
cargo clippy
cargo test
```

Web changes have additional checks in [web/AGENTS.md](web/AGENTS.md).

Add `ci-macos` when a PR needs macOS coverage; a skipped check is not coverage.
The [labeler](.github/workflows/labeler.yml) requests it conservatively for
sensitive changes or potentially incomplete file data. It runs from the base
branch, so new or changed detection rules may need a manual label.

Releases are staged weekly. Maintainer instructions live in
[docs/development/releases.md](docs/development/releases.md).

For help, open a [GitHub Discussion](../../discussions) or an issue.
