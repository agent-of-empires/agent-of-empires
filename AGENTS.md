# Repository Guidelines

> `CLAUDE.md` is a symlink to this file. Do not edit `CLAUDE.md` directly; edit `AGENTS.md` instead.

## Project Structure & Module Organization

Most of the tree is self-describing; the entries below carry context that reading
the code alone would not give you.

- `src/process/`: OS-specific process handling (`macos.rs`, `linux.rs`) plus `worker.rs`, the protocol-agnostic worker-subprocess substrate (process-group signalling, liveness, on-disk worker paths) that the plugin host will reuse, and the ACP worker layer built on it that `src/acp/` consumes: `worker_registry.rs` (on-disk registry of detached ACP workers) and `runner.rs` (the `aoe __acp-runner` shim that owns an agent subprocess and outlives `aoe serve`).
- `src/events/`: protocol-agnostic durable event-log storage core (topic-keyed SQLite seq log, retention, keyset scans, attachments); `src/acp/`'s `EventStore` is the first consumer.
- `src/migrations/`: versioned data migrations for breaking changes (see below).
- `tests/e2e/`: end-to-end tests exercising the full `aoe` binary (see E2E Tests below).
- `docs/development/adding-agents.md`: guide for adding a new agent to AoE.
- `docs/development/adding-settings.md`: guide for adding a setting via the single-source schema.
- `aoe-plugin-api/`: plugin manifest and capability types (see `docs/development/internals/plugin-system.md`).
- `contrib/`: community-maintained integration files (e.g., OpenClaw skill). Checked by `cargo xtask check-skill` in CI.

## Build, Test, and Development Commands

- `cargo build` / `cargo build --release`: TUI-only (release binary at `target/release/aoe`).
- `cargo build --profile dev-release`: optimized local builds without LTO; faster compile. Lands on the release namespace (app dir, tmux prefix, serve port), so it shares state with an installed release `aoe`. Use `--release` only when producing a shipping binary.
- `cargo build --features serve`: includes the web dashboard (needs Node.js + npm).
- `cargo test`: unit + integration tests (some skip if `tmux` unavailable).
- `cargo fmt` + `cargo clippy`: run before pushing; fix clippy warnings unless there's a strong reason not to.
- Debug logging: `AGENT_OF_EMPIRES_DEBUG=1 cargo run` (writes `debug.log` in app data dir).
- Running from source needs `tmux` installed.
- Debug builds use an isolated namespace so they don't collide with an installed release `aoe`: app data dir is `~/.agent-of-empires-dev` (macOS/Windows) or `~/.config/agent-of-empires-dev` (Linux), tmux session prefix is `aoe_dev_`, and `aoe serve` defaults to port `8081`. Release builds keep the original `agent-of-empires` paths, `aoe_` prefix, and port `8080`.
- Debug builds also run tmux on their own socket (`<app_dir>/tmux.sock`) rather than tmux's shared default, so a `cargo run` / e2e build can never poison an installed release build's tmux server (default-shell, base env); release builds keep the default socket. Set `AOE_TMUX_SOCKET=<path>` to force a specific socket (the e2e harness uses this to isolate each test).

### Web Dashboard

Build it with `cargo build --features serve` (needs Node.js + npm); a plain
`cargo build` is TUI-only and needs no JS tooling. Build/run/dev-server recipes,
the oxfmt-not-prettier CI gate, and the Playwright + Vitest suites are in
`web/AGENTS.md` (loaded via its `web/CLAUDE.md` symlink when you work under
`web/`).

## Settings & Configuration

Settings are single-source (#1692): a field is declared once on its `Config`
sub-struct and every surface derives from that declaration. Adding a setting is
one edit, the `#[setting(...)]` annotation on the field:

```rust
/// Doc comment becomes the field's description on every surface.
#[serde(default)]
#[setting(label = "My Setting", widget = "toggle")]
pub my_setting: bool,
```

Everything else derives from that one declaration: the TUI rows, the web
FormFields, the server-side PATCH validation, and profile/repo overrides. There
is no `FieldKey`, `build_*_fields`, `apply_field_*`, or `*ConfigOverride` struct
to extend, so don't add one.

**`docs/development/adding-settings.md` is the reference** for the full attribute
list, choosing a section and widget, custom widgets, and what stays out of the
schema. Read it before adding a setting rather than inferring the attributes.

## Coding Style & Naming Conventions

- Let `cargo fmt` + `cargo clippy` decide; fix warnings.
- **No dead code.** Never add `#[allow(dead_code)]` or write fields/functions that nothing reads. If a field isn't used yet, don't add it; if it stops being used, remove it.
- **No emdashes or `--`** as separators in docs/comments; use commas, semicolons, or rephrase. The rule applies to human-authored prose only; auto-generated content inherits whatever its renderer emits, so leave those files alone.
- Keep OS-specific logic in `src/process/{macos,linux}.rs`, not sprinkled `cfg` checks.
- Don't preserve backwards compatibility by default; call it out when a change is breaking.
- Comments: explain non-obvious "why"; skip section headers and comments that restate the code.

## Testing Guidelines

- Use unit tests in-module (`#[cfg(test)]`) for pure logic; use `tests/*.rs` for integration tests.
- Tests must be deterministic and clean up after themselves (tmux tests should use unique names like `aoe_test_*` or `aoe_e2e_*`).
- Avoid reading/writing real user state; prefer temp dirs (see `tempfile` usage in `src/session/storage.rs`).
- New features touching TUI rendering, CLI subcommands, or session lifecycle should consider adding an e2e test.

### What a test costs

A test is not free, and its dominant cost is usually not its runtime. The
`#[cfg(test)]` code in `src/` is 33% of the crate's lines and **65% of the
crate's compile time** (measured: 42s to rebuild the lib without test code,
119s with, deps warm). That compile is paid by six Rust CI jobs on every PR,
while the whole 5,700-test unit suite *executes* in 75s. So the thing to
economize is test **code volume and test-fn count**, not assertions.

Rough cost per test at each tier, so the choice is informed:

| Tier | Cost per test | Notes |
| --- | --- | --- |
| Rust unit / Vitest | ~15-30ms run, plus compile weight | default choice |
| Playwright mocked | ~6.3s | ~1.9s of it is one page load |
| Playwright live | ~9s | spawns a real `aoe serve` |
| Rust e2e | ~2s median, 10-28s for live-daemon ACP | strictly serial; adds to the critical path forever |

Pick the cheapest tier that can actually fail if the behavior breaks. A mocked
Playwright test costs roughly 200x a Vitest test; reach for it only when the
assertion needs a real browser (focus, keyboard, drag-drop, touch, viewport).

### One test per behavior, not per input

Input permutations belong in a table inside one test, not in a test per case.
Each extra `#[test]` fn is a symbol, a codegen unit contribution, and compile
time; each extra table row is one line. This is the house style:

```rust
#[test]
fn test_format_tmux_prefix() {
    let cases = [("C-a", "Ctrl+a"), ("M-x", "Alt+x"), ("", "Ctrl+b")];
    for (input, expected) in cases {
        assert_eq!(format_tmux_prefix(input), expected, "{input:?}");
    }
}
```

Keep a case's explanatory comment as a comment on its row; the assertion count
should not drop when you consolidate. Split a case back out into its own test
only when it needs different setup (a guard, `#[serial]`, an env var).

### Don't write these

Coverage percentage is a diagnostic, not a target. Do not add a test whose only
effect is to move the number:

- **Constants and default values.** `assert_eq!(SOME_CONST, 5)` and
  `assert_eq!(Config::default().flag, false)` restate the declaration; they
  fail only when someone deliberately edits both. A test that pins a
  *relationship* between constants (`PONG_IDLE_TIMEOUT > PING_INTERVAL`) or an
  invariant across a table (migrations are sequential) is worth keeping, because
  it fails when someone changes one side and forgets the other.
- **Derived impls.** `Debug`, `Clone`, `PartialEq`, and serde round-trips on a
  plain data struct test the derive macro, not our code. Test `Display` only
  when the string is a user-facing contract.
- **One test per enum variant** over a total `match`. The compiler already
  proves exhaustiveness; use a table for the variants whose mapping is
  non-obvious.
- **"Renders without crashing"** smoke tests for a component that a behavior
  test in the same file already mounts.
- **Getters and setters** with no logic in them.

If a change genuinely does not warrant a new test, say so in the PR description
rather than adding a tautological one to satisfy `codecov/patch`. Reviewers
should treat "added a test that cannot fail" as a review comment.

### E2E Tests

Full-binary e2e tests live in `tests/e2e/`, exercising `aoe` through tmux (TUI) and as a subprocess (CLI). Run with `cargo test --features e2e-tests --test e2e` (add `-- --nocapture` for screen dumps on failure). The e2e target is gated behind the `e2e-tests` feature so CI can run the serve suite as parallel shards (one runs everything except e2e, one runs e2e only); `cargo test --features serve` skips e2e, and naming the target without the feature errors loudly instead of skipping. Run the full serve suite locally with `cargo test --features serve,e2e-tests`.

The harness (`tests/e2e/harness.rs`) exposes `TuiTestHarness` with `spawn_tui()`/`spawn(args)`, `send_keys(keys)`/`type_text(text)`, `wait_for(text)` (10s timeout), `capture_screen()`/`assert_screen_contains(text)`, and `run_cli(args)`. TUI tests auto-skip without tmux; Docker tests use `#[ignore]`.

**Use `#[parallel]`, not `#[serial]`, on a new e2e test.** Isolation does not come from serialization: the harness gives each test its own tempdir `$HOME`, its own tmux socket and session name, and passes `HOME` / `XDG_CONFIG_HOME` / `AOE_TMUX_SOCKET` explicitly on every `Command` it spawns. The suite runs at `--test-threads=3` in CI (336s serial -> 114s), and the work is latency-bound on polling tmux panes rather than CPU-bound, so extra concurrency is close to free.

`#[serial]` (default key) is reserved for the few tests that mutate **process-global** state, which today means `HomeGuard` callers (`filewatch_tui_*`) and `update_command.rs` (`set_var("AOE_UPDATE_BASE_URL")`). `serial_test` guarantees a default-key `#[serial]` test never overlaps a default-key `#[parallel]` one, and that guarantee is what makes the `unsafe` env mutation in `HomeGuard` sound. If a test needs an isolated `$HOME` only for its *subprocesses*, it does not need `HomeGuard` or `#[serial]` at all. A named `#[serial(key)]` group (e.g. `file_watch`) only excludes other tests sharing that key, so it is not a substitute.

Agent-view live-daemon e2e (`tests/e2e/acp_focus_isolation_e2e.rs`) stands up a real `aoe serve --daemon` and attaches the native TUI structured view against it. It reuses the shared Node fake-ACP agent (`web/tests/helpers/fakeAcpAgent.mjs`) to drive a deterministic pending approval, so it needs `--features serve` and Node on `PATH` (it auto-skips via `require_node!` otherwise). The harness installs the fake as the `claude` / `claude-agent-acp` / `aoe-agent` shims (`install_acp_shim`), roots `$HOME` under `/tmp` (`new_in_tmp`, keeping the worker unix socket under the macOS `sun_path` limit), and stops the worker plus daemon on `Drop` (`stop_daemon_on_drop`).

Recording (for PR reviews): `RECORD_E2E=1 cargo test --features e2e-tests --test e2e -- --nocapture` locally (needs `asciinema` + `agg`, outputs to `target/e2e-recordings/`), or add the `needs-recording` label in CI.

### Web Dashboard Playwright Tests

Two suites (mocked and live), which one to pick, the coverage-matrix mandate, and
the mobile/touch recipe are in `web/AGENTS.md`.

## Commit & Pull Request Guidelines

- Branch names: `feature/...`, `fix/...`, `docs/...`, `refactor/...`.
- Commit messages: use conventional commit prefixes (`feat:`, `fix:`, `docs:`, `refactor:`).
- PRs: follow the template in `.github/pull_request_template.md`. When creating PRs via `gh pr create`, read the template first and use its structure for the `--body` argument. Include a clear “what/why”, how you tested (`cargo test`, plus any manual tmux/TUI checks), and screenshots/recordings for UI changes.

### Definition of done

Before requesting review, every PR must clear:

1. **`cargo fmt`, `cargo clippy`, `cargo test`** all clean (`--features serve` if the change touches the web dashboard or structured view). For any `web/` change, also **`cd web && npm run format:check && npm run lint`** (oxfmt + ESLint; both are CI gates, and neither ESLint nor tsc catches formatting).
2. **Web tests when applicable.** If the change touches a user-facing dashboard flow listed in the coverage matrix mandate (auth, wizard, settings, profiles, sessions / sidebar, right panel / diff / notifications, directory browser, devices, git clone, connectivity, read-only), update `web/tests/coverage-matrix.json` and add or modify the appropriate Vitest / Playwright test. CI fails on a missing matrix entry.
3. **Codecov checks.** See below.

### Codecov requirements

Coverage runs on every PR via the merge of Vitest + Playwright LCOVs (see `web/scripts/merge-coverage.mjs`). Current scope is `web/` only; a Rust backend coverage flag is queued as follow-up.

**Two checks gate merges:**

- **`codecov/patch`** (target: 75%). The lines your PR adds or changes must hit 75% coverage. This is the strict gate, sized so a small frontend PR with one missed line still passes.
- **`codecov/project`** (target: auto). Overall repo coverage must not drop below `main`'s current level by more than the 1% threshold.

**Components show up in the PR comment, not as status checks.** `codecov.yml` sets `component_management.default_rules.statuses: []`, so the per-component slices (App Shell, Auth, Structured View UI, etc.) appear under the components table in the Codecov PR comment but never post a separate GitHub status. The repo-wide `codecov/patch` and `codecov/project` checks are the only Codecov gates on the merge box. The component baselines are still being lifted by the foundation follow-ups (#1217 through #1224, threshold enforcement tracked in #1225); when you touch one of those surfaces, add tests that improve its number, but don't chase the comment-only component numbers on unrelated PRs.

**Rust-only PRs.** Patch coverage is reported against `web/src/**` paths only, so a Rust-only diff is N/A for patch coverage and inherits the previous flag value via `carryforward: true`. The aggregate `codecov/patch` and `codecov/project` checks pass.

## Git Configuration

- Do not modify git configuration (e.g., `.gitconfig`, `.git/config`, `git config` commands) without explicit user approval.
- The one exception: adding a new remote to fetch a contributor's fork during PR code review is allowed without asking.

## Local Data & Configuration Tips

- Runtime config/data location:
  - **Linux**: `$XDG_CONFIG_HOME/agent-of-empires/` (defaults to `~/.config/agent-of-empires/`)
  - **macOS**: `~/.agent-of-empires/` by default, or `$XDG_CONFIG_HOME/agent-of-empires/` when `XDG_CONFIG_HOME` is set or that dir already exists (issue #1948). Resolution is `get_app_dir_path` -> `macos_app_dir`; nothing is moved automatically, so an existing `~/.agent-of-empires/` keeps being used.
  - **Windows**: `~/.agent-of-empires/`
- Keep user data out of commits. For repo-local experiments, use ignored paths like `./.agent-of-empires/`, `.env`, and `.mcp.json`.
- `aoe serve` writes several files to the app dir while running. All are owner-only (0600) where they contain secrets. The daemon cleans them up on shutdown; `daemon_pid()`'s stale-PID check sweeps them otherwise.
  - `serve.pid`: daemon PID for `--stop` and reattach detection.
  - `serve.url`: primary URL (includes the auth token) plus alternates.
  - `serve.mode`: `tunnel` / `tailscale` / `local`.
  - `serve.passphrase`: plaintext Tunnel passphrase, so the TUI can show it on reopen across restarts.
  - `serve.last_mode`, `serve.last_port`: picker defaults across launches.
  - `login_sessions.toml`: persisted dashboard login sessions (0600), so signed-in devices survive a daemon restart (#1235). Unlike the `serve.*` files it is intentionally NOT cleaned up on `--stop`; that would reproduce the re-prompt bug. Dropped on a passphrase change; gated by `auth.persist_sessions`.

Daemon tracing and stdout/stderr now land in the configured `[logging].file_path` (default `~/.agent-of-empires/debug.log`) alongside the TUI and structured-view runners; see `docs/development/logging.md` for sinks and rotation.

## Data Migrations

Breaking changes to stored data (file locations, config schema) go through `src/migrations/`, not inline fallback/compat shims. A `.schema_version` file tracks state; `migrations::run_migrations()` runs pending ones in order on startup and bumps the version.

Step-by-step recipe: `docs/development/adding-a-migration.md`.

`docs/cli/reference.md` is auto-generated by `cargo xtask gen-docs`; edit the clap help in `src/cli/` and re-run instead. CI enforces it.

## Embedded Assets and the Nix Build

Every compile-time embedded asset (`include_bytes!`, `include_str!`, `include!`) that is not a `*.rs` or `*.toml` must be unioned into `commonArgs.src` in `flake.nix`; otherwise the Cargo source filter drops it and `nix build` fails on it while `cargo build` stays green (#3204). `scripts/check-nix-embedded-assets.py` validates this on every PR.

## Website & Documentation

The public website (agent-of-empires.com) is an Astro static site in `website/`.

- **`docs/`** is the canonical source for all documentation and guide content. Edit docs here, never on the website side.
- Astro component pages (`*.astro`) like `website/src/pages/guides/index.astro` are not generated; edit them directly.

To add a new page to the website, follow `docs/development/adding-a-website-page.md`.

## Design System

Read `DESIGN.md` before any visual/UI change — fonts, colors, spacing, and aesthetic direction are defined there. Don't deviate without explicit approval; in QA mode, flag code that doesn't match.

## Skill routing

When the user's request matches an available skill, ALWAYS invoke it using the Skill
tool as your FIRST action. Do NOT answer directly, do NOT use other tools first.
The skill has specialized workflows that produce better results than ad-hoc answers.

Key routing rules:
- Product ideas, "is this worth building", brainstorming → invoke office-hours
- Bugs, errors, "why is this broken", 500 errors → invoke investigate
- Ship, deploy, push, create PR → invoke ship
- QA, test the site, find bugs → invoke qa
- Code review, check my diff → invoke review
- Update docs after shipping → invoke document-release
- Weekly retro → invoke retro
- Design system, brand → invoke design-consultation
- Visual audit, design polish → invoke design-review
- Architecture review → invoke plan-eng-review
- Save progress, checkpoint, resume → invoke checkpoint
- Code quality, health check → invoke health
