# Contributing to aoe

Thanks for your interest in contributing to aoe (Agent of Empires)! This document provides guidelines for contributing to the project.

## Before You Start

- Search existing [issues](../../issues) and [pull requests](../../pulls) to avoid duplicates
- For significant changes (new features, architectural modifications), please open an issue first to discuss the approach
- Read the [Code of Conduct](CODE_OF_CONDUCT.md)

## Development Setup

### Prerequisites

- **Rust**: Install via [rustup](https://rustup.rs/)
- **tmux**: Required for running the application (`brew install tmux` on macOS, `apt install tmux` on Ubuntu)
- **Git**: For version control
- **Node.js + npm**: Needed for the default build, which includes the web dashboard. TUI-only development can skip JS tooling with `cargo build --no-default-features`.
- **[kache](https://github.com/kunobi-ninja/kache)** (optional): An opt-in rustc wrapper that shares dependency builds across worktrees to cut compile time and disk. Not required to build. If you keep several worktrees in flight, see [Faster rebuilds across worktrees](docs/development.md#faster-rebuilds-across-worktrees-kache).

### Quick Start

```bash
# Fork the repo on GitHub, then clone your fork
git clone https://github.com/YOUR_USERNAME/agent-of-empires.git
cd agent-of-empires

# Add upstream remote
git remote add upstream https://github.com/ORIGINAL_OWNER/agent-of-empires.git

# Build and run
cargo build --release
cargo run --release
```

### Useful Commands

```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo build --profile dev-release  # Fast optimized build for local dev
cargo check                    # Fast type-checking
cargo test                     # Run tests
cargo fmt                      # Format code
cargo clippy                   # Lint
```

For debug logging (writes to `debug.log` in app data dir):
```bash
AGENT_OF_EMPIRES_DEBUG=1 cargo run
```

## Making Changes

### Branch Naming

Use descriptive branch names with prefixes:
- `feature/...`: New features
- `fix/...`: Bug fixes
- `docs/...`: Documentation changes
- `refactor/...`: Code refactoring

### Code Style

- Run `cargo fmt` before committing
- Fix `cargo clippy` warnings unless there's a strong reason not to
- Follow Rust naming conventions: `snake_case` for functions/modules, `CamelCase` for types
- Keep OS-specific logic in `src/process/{macos,linux}.rs`

See [CLAUDE.md](CLAUDE.md) for detailed coding guidelines and project structure.

### Commit Messages

Use conventional commit prefixes:
- `feat:`: New features
- `fix:`: Bug fixes
- `docs:`: Documentation
- `refactor:`: Code refactoring
- `test:`: Test changes
- `chore:`: Build/tooling changes

Example: `feat: add session export command`

### Changelog visibility

`CHANGELOG.md` and the GitHub Release body are generated from conventional commit messages on `main` by [git-cliff](https://git-cliff.org/) (config in [`cliff.toml`](cliff.toml)). Squash-merged PR titles are what gets parsed, so PR titles matter.

User-visible prefixes appear in release notes:

- `feat:` new user-visible behavior
- `fix:` bug fixes that affect users
- `perf:` performance improvements
- `security:` security fixes
- `revert:` reverts of previously released changes

Routine maintenance is intentionally hidden:

- `chore:`, `chore(deps):`, `chore: update Nix npmDepsHash`
- `build:`, `ci:`
- `docs:`, `style:`
- `refactor:`, `test:`

For web-dashboard changes, scope visibility via the prefix: `feat(web): ...` / `fix(web): ...` show up; `refactor(web):` / `chore(web):` / `test(web):` stay out. Same pattern for `acp`, `serve`, `tui`.

A non-conventional PR title no longer disappears silently from the changelog: `cliff.toml`'s catch-all parser routes anything without a recognized prefix into a generic "Other" group, and the release workflow fails if git-cliff still flags a parse-error skip. Even so, the **PR Title Check** workflow runs on every PR and refuses to pass until the title parses as `<type>(<scope>)?: <subject>` with a lowercase subject, so "Other" should only ever catch direct pushes to `main`. Reword titles like "Fix stuff" to `fix: <thing>` before merging.

## Testing

- Run `cargo test` before submitting PRs
- Tests should be deterministic and clean up after themselves
- tmux-related tests use unique names prefixed with `aoe_test_*`
- For TUI changes, test manually in a real terminal

## Submitting Pull Requests

1. Push your branch to your fork
2. Open a pull request against the `main` branch
3. Fill out the PR template completely
4. Ensure CI checks pass

### What to Include

- Clear description of what changed and why
- How you tested the changes
- Screenshots/recordings for UI changes
- Link to related issues

## Releases

`agent-of-empires` releases at least weekly via an automated staging PR (Wednesday 09:00 UTC) that the maintainer reviews and merges. See [`docs/development/releases.md`](docs/development/releases.md) for the full cadence, the post-merge tagger flow, and how emergency releases work.

## Your First Contribution

New to the project? Here are some ways to get started:

- Look for issues labeled `good-first-issue` or `help-wanted`
- Fix typos or improve documentation
- Add tests for existing functionality
- Try the app and report bugs

Don't hesitate to ask questions in issues or PRs. Every contributor started somewhere!

## Questions?

Open a [GitHub Discussion](../../discussions) or file an issue.
