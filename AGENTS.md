# Repository Guidelines

> `CLAUDE.md` links to this file. Edit `AGENTS.md` only.

## Where to look

- `src/process/`: OS process handling plus reusable worker supervision.
- `src/events/`: durable topic event log used by ACP and plugins.
- `src/migrations/`: versioned migrations for persisted data.
- `tests/e2e/`: full-binary tests. See [E2E tests](#e2e-tests).
- `aoe-plugin-api/`: public plugin manifest and capability types.
- `docs/development/`: contributor and architecture references.

## Commands

```sh
cargo build                         # TUI + daemon, no Node needed
cargo build --features web          # web dashboard, requires Node and npm
cargo build --profile dev-release   # optimized local build without LTO
cargo test
cargo fmt
cargo clippy
AGENT_OF_EMPIRES_DEBUG=1 cargo run
```

Debug builds use an isolated app directory, tmux socket and `aoe_dev_` session
prefix, with `aoe serve` on port 8081. Release and `dev-release` builds use the
installed app's namespace and port 8080. `AOE_TMUX_SOCKET` overrides the socket.

For web commands and tests, read `web/AGENTS.md`.

## Code rules

- Let `cargo fmt` and `cargo clippy` decide style; fix warnings.
- Do not add dead code or `#[allow(dead_code)]`.
- Keep comments short and precise. Add one only when the code cannot clearly express a
  non-obvious reason or invariant; do not restate code or preserve
  implementation history.
- Leave the comments around a change shorter than you found them: prune
  narration, repeated rationale, and implementation history as you touch them.
- Add standalone documentation only for a durable user workflow, public
  contract, or cross-cutting invariant. Keep implementation details with the
  code; do not add feature inventories, rollout history, or duplicate guides.
- Link to one canonical source instead of restating it. Delete or update stale
  documentation in the same change that makes it stale.
- Do not use em dashes or `--` as prose separators in hand-written docs and
  comments.
- Keep OS-specific behavior in `src/process/{macos,linux}.rs`.
- Breaking changes do not need compatibility shims unless explicitly requested.

Settings are declared once on their `Config` field with `#[setting(...)]`.
Read `docs/development/adding-settings.md` before adding one; do not create
parallel field registries or override structs.

## Tests

Use in-module unit tests for pure logic and `tests/` for integration behavior.
Tests must be deterministic, isolated from user state, and clean up resources.
Use table cases inside one test when setup is shared. Do not test constants,
derived implementations, trivial getters, or rendering without an asserted
behavior.

Choose the cheapest test that can catch the regression. Rust unit tests and
Vitest are preferred; browser and full-binary tests are for behavior that needs
those environments. Test code is a major part of Rust compile time, so avoid
duplicating cases across test functions.

### E2E tests

Run `cargo test --features e2e-tests --test e2e`; add `web` for dashboard
coverage. New e2e tests use `#[parallel]`. Reserve `#[serial]` for tests
that mutate process-global state. The harness already isolates HOME,
tmux sockets, and session names. Tests auto-skip unavailable external tools.

Use `RECORD_E2E=1` to record local review artifacts. See `tests/e2e/harness.rs`
for the harness API.

## Pull requests

- Branch prefixes: `feature/`, `fix/`, `docs/`, `refactor/`.
- Use conventional commit and PR titles.
- Follow `.github/pull_request_template.md`; include what changed, why, tests,
  and screenshots or recordings for UI changes.
- Before review, run `cargo fmt`, `cargo clippy`, and `cargo test`, adding
  `--features web` when relevant.
- For `web/` changes also run its format, lint, type, and applicable test checks
  from `web/AGENTS.md`, including the coverage matrix requirement.

Codecov gates web changes at 75% patch coverage and prevents a project drop
beyond its threshold. Do not add tautological tests merely to move coverage.
Rust-only changes carry the web flag forward.

Do not change git configuration without explicit approval. Adding a contributor
fork remote for review is the only exception.

## Stored data and generated content

Persisted schema or path changes require a migration in `src/migrations/`; see
`docs/development/adding-a-migration.md`. Do not add inline compatibility paths.

`docs/cli/reference.md` is generated from clap help via `cargo xtask gen-docs`.
Edit the CLI source and regenerate it. Documentation content lives in `docs/`;
the website consumes it. Follow `docs/development/adding-a-website-page.md` for
new pages.

Every non-Rust, non-TOML asset embedded with `include_*` must be added to
`commonArgs.src` in `flake.nix`; CI checks this with
`scripts/check-nix-embedded-assets.py`.

Runtime state belongs in the platform app directory, never in commits. Use
ignored paths for experiments.

Read `DESIGN.md` before visual changes.

## Skills

When an available skill matches the request, invoke it before doing the work.
