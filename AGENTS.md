# Repository Guidelines

> `CLAUDE.md` is a symlink to this file. Do not edit `CLAUDE.md` directly; edit `AGENTS.md` instead.

## Project Structure & Module Organization

- `src/main.rs`: binary entrypoint (`aoe`).
- `src/lib.rs`: shared library code used by the CLI/TUI.
- `src/cli/`: clap command handlers (e.g., `src/cli/add.rs`, `src/cli/session.rs`).
- `src/tui/`: ratatui UI and input handling.
- `src/session/`: session storage, configuration, and group management.
- `src/tmux/`: tmux integration and status detection.
- `src/process/`: OS-specific process handling (`macos.rs`, `linux.rs`).
- `src/docker/`: Docker sandboxing and container management.
- `src/git/`: git worktree operations and template resolution.
- `src/server/`: web dashboard backend (axum server, REST API, WebSocket PTY relay, auth).
- `src/update/`: version checking against GitHub releases.
- `web/`: React + TypeScript frontend for the web dashboard (built with Vite + Tailwind CSS).
- `src/migrations/`: versioned data migrations for breaking changes (see below).
- `tests/`: integration tests (`tests/*.rs`).
- `tests/e2e/`: end-to-end tests exercising the full `aoe` binary (see E2E Tests below).
- `docs/`: user-facing documentation and guides.
- `docs/development/adding-agents.md`: guide for adding a new agent to AoE.
- `docs/development/adding-settings.md`: guide for adding a setting via the single-source schema.
- `scripts/`: installation and utility scripts.
- `xtask/`: build automation workspace.

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

### Web Dashboard

- Stack: React 19, TypeScript, Vite, Tailwind v4, xterm.js v6. Installable as a PWA ("Install Agent of Empires" in Chrome; "Add to Home Screen" on iOS).
- Build: `cargo build --features serve` (build.rs runs `npm install && npm run build` in `web/` when inputs change).
- Run: `aoe serve --host 0.0.0.0` (token-based auth by default).
- Frontend dev: `cargo xtask dev` (Unix) builds the serve binary, then runs `aoe serve` (8081) and the Vite dev server (5173, HMR) together, pointing Vite at the backend via `VITE_PROXY` so `/api` and the `/sessions/*/ws` relays resolve; open `:5173`, Ctrl-C stops both. Or run them by hand: `cd web && npm run dev` plus a separate `cargo run --features serve -- serve`.
- TUI-only `cargo build` (without `--features serve`) needs no JS tooling.

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

`#[derive(SettingsSection)]` (the `aoe-settings-derive` crate) turns each
annotated field into a `FieldDescriptor` in `settings_schema::schema()`. From
there everything is automatic:

- **TUI** builds its rows from the schema (`src/tui/settings/fields.rs`); reads
  and writes go through the serialized `Config` JSON and the generic
  `merge_json` / `clear_path`. No `FieldKey`, `build_*_fields`, or
  `apply_field_*` to touch.
- **Web** fetches `GET /api/settings/schema` and renders generic FormFields
  (`web/src/components/settings/SchemaSection.tsx`). Every config-backed
  section is schema-driven; `custom:<id>` fields resolve through the web
  custom-widget registry (`web/src/components/settings/customWidgetRegistry.ts`)
  and a section may pass an `onAfterSave` hook for cross-surface effects (the
  acp section refreshes `serverAbout`). Only `diff` (client-local) and
  `telemetry` (separate consent endpoint) stay hand-written.
- **Server** validates each PATCH leaf against the schema's `web_write` policy
  and `validation` rule (`settings_schema::validate_patch`); no hand-kept
  allowlist.
- **Profile/repo overrides** are stored as sparse JSON and merged generically,
  so there is no `*ConfigOverride` struct or merge arm to extend.

Attribute keys: `label`, `desc` (defaults to the doc comment), `widget`
(`toggle` / `text` / `optional_text` / `number` / `slider` / `select` /
`list` / `custom:<id>`), `options` (for `select`, `value:Label,...`),
`min` / `max` / `step`, `validate` (`range:MIN[:MAX]` / `nonempty` /
`memory_limit` / `volume_list` / `env_list` / `port_mapping_list`), `web`
(`elevation:<reason>` /
`local_only:<reason>`; omit for plain allow), `category` (override the
section's default tab), `advanced` (group under an Advanced fold), `global_only`
(shown but not profile-overridable), and `skip` (exclude from the schema). The
section itself is declared with `#[setting_section(name = "...", category =
"...")]`. A `custom:<id>` widget keeps a bespoke control: register the id in the
TUI custom-widget map AND the web one
(`web/src/components/settings/customWidgetRegistry.ts`); an unregistered web id
renders a visible "no control" placeholder rather than silently dropping the
field. `Config.environment` (the host env list) stays a TUI-only extra row: it
is a root-level `Vec<String>` with no `SettingsSection`, so schematizing it
would need a breaking config-layout migration; the web does not surface it.

## Coding Style & Naming Conventions

- Let `cargo fmt` + `cargo clippy` decide; fix warnings.
- **No dead code.** Never add `#[allow(dead_code)]` or write fields/functions that nothing reads. If a field isn't used yet, don't add it; if it stops being used, remove it.
- **No emdashes or `--`** as separators in docs/comments; use commas, semicolons, or rephrase. The rule applies to human-authored prose only; auto-generated content inherits whatever its renderer emits, so leave those files alone.
- Rust naming: `snake_case` modules/functions, `CamelCase` types, `SCREAMING_SNAKE_CASE` constants.
- Keep OS-specific logic in `src/process/{macos,linux}.rs`, not sprinkled `cfg` checks.
- Don't preserve backwards compatibility by default; call it out when a change is breaking.
- Comments: explain non-obvious "why"; skip section headers and comments that restate the code.

## Testing Guidelines

- Use unit tests in-module (`#[cfg(test)]`) for pure logic; use `tests/*.rs` for integration tests.
- Tests must be deterministic and clean up after themselves (tmux tests should use unique names like `aoe_test_*` or `aoe_e2e_*`).
- Avoid reading/writing real user state; prefer temp dirs (see `tempfile` usage in `src/session/storage.rs`).
- New features touching TUI rendering, CLI subcommands, or session lifecycle should consider adding an e2e test.

### E2E Tests

Full-binary e2e tests live in `tests/e2e/`, exercising `aoe` through tmux (TUI) and as a subprocess (CLI). Run with `cargo test --test e2e` (add `-- --nocapture` for screen dumps on failure).

The harness (`tests/e2e/harness.rs`) exposes `TuiTestHarness` with `spawn_tui()`/`spawn(args)`, `send_keys(keys)`/`type_text(text)`, `wait_for(text)` (10s timeout), `capture_screen()`/`assert_screen_contains(text)`, and `run_cli(args)`. TUI tests auto-skip without tmux; Docker tests use `#[ignore]`; all use `#[serial]` for tmux isolation.

Agent-view live-daemon e2e (`tests/e2e/acp_focus_isolation_e2e.rs`) stands up a real `aoe serve --daemon` and attaches the native TUI structured view against it. It reuses the shared Node fake-ACP agent (`web/tests/helpers/fakeAcpAgent.mjs`) to drive a deterministic pending approval, so it needs `--features serve` and Node on `PATH` (it auto-skips via `require_node!` otherwise). The harness installs the fake as the `claude` / `claude-agent-acp` / `aoe-agent` shims (`install_acp_shim`), roots `$HOME` under `/tmp` (`new_in_tmp`, keeping the worker unix socket under the macOS `sun_path` limit), and stops the worker plus daemon on `Drop` (`stop_daemon_on_drop`).

Recording (for PR reviews): `RECORD_E2E=1 cargo test --test e2e -- --nocapture` locally (needs `asciinema` + `agg`, outputs to `target/e2e-recordings/`), or add the `needs-recording` label in CI.

### Web Dashboard Playwright Tests

Two suites under `web/`:

- **Mocked**: `web/tests/*.spec.ts`, run via `cd web && npx playwright test --config=playwright.config.ts`. Uses `page.route()` to stub `/api/*` responses; serves the production Vite bundle through `vite preview` on port 4173. Fast and deterministic; for UI logic that does not depend on real backend state.
- **Live**: `web/tests/live/*.spec.ts`, run via `cd web && npx playwright test --config=playwright.live.config.ts`. Each test spawns a real `aoe serve` subprocess against an isolated `HOME` via the harness in `web/tests/helpers/aoeServe.ts`. Two workers, `workerIndex`-based port allocation, `TMUX_TMPDIR` per test. For flows that depend on backend persistence, auth, sessions, tmux, git, read-only, or structured view.

When deciding which suite to use:

- Backend, persistence, auth, session, tmux, git, read-only, or structured-view round-trip flows belong in **live Playwright**.
- Request-payload permutations (does control X emit the right JSON keys) belong in **Vitest + RTL + MSW** under `web/src/**/__tests__/`. See `web/src/components/settings/__tests__/SoundSettings.test.tsx` as the canonical example.
- Browser-specific behavior not practical in Vitest (focus, keyboard, drag-drop, modal escape, mobile viewport, touch events) belongs in **mocked Playwright**.

**Mandate:** any PR that changes a user-facing dashboard flow under auth, wizard / session creation, settings, profiles, sessions / sidebar, right panel / diff / notifications, directory browser, devices, git clone, connectivity, or read-only behavior must update `web/tests/coverage-matrix.json` and add or modify the appropriate test. CI fails on a missing matrix entry via `web/tests/validate-coverage-matrix.mjs`. Pure styling or copy-only changes may add a `kind: "deferred"` entry with a `reason` and a linked issue.

**Coverage reports.** Vitest uses `@vitest/coverage-v8`; Playwright uses `vite-plugin-istanbul` gated by `AOE_COVERAGE=1`. The merge script (`web/scripts/merge-coverage.mjs`, via `npm run coverage:merge`) feeds both into `monocart-coverage-reports` and writes `web/coverage/merged/` (LCOV + HTML + summary). The CI `coverage` job posts a PR comment with deltas via `davelosert/vitest-coverage-report-action`; baseline is the most recent main-branch artifact.

Full recipe, harness API, and fake-ACP-agent details live in `docs/development/playwright.md`.

**Legacy mobile/touch recipe (still applies for the mocked specs under `tests/`):**

1. Shim a fake tool on `$PATH` so `aoe add --cmd claude` creates a live tmux session (the live harness already does this for you).
2. Emulate mobile with `devices['iPhone 13']` so `pointer: coarse` matches.
3. Spy on PTY bytes by patching `WebSocket.prototype.send` in an `addInitScript` and pushing into `window.__WS_SENT__`.
4. Synthesize multi-touch via `page.evaluate` dispatching raw `new TouchEvent(...)` on `.xterm`; Playwright's `page.touchscreen` is single-finger only.

**Gotcha:** synthetic touchmove events fire back-to-back with Δt≈1ms, which blows up any `Δpx / Δtime` velocity calculation. Cap velocity and per-frame emit counts, or a real device will look sane while the e2e produces runaway momentum (or vice-versa).

## Commit & Pull Request Guidelines

- Branch names: `feature/...`, `fix/...`, `docs/...`, `refactor/...`.
- Commit messages: use conventional commit prefixes (`feat:`, `fix:`, `docs:`, `refactor:`).
- PRs: follow the template in `.github/pull_request_template.md`. When creating PRs via `gh pr create`, read the template first and use its structure for the `--body` argument. Include a clear “what/why”, how you tested (`cargo test`, plus any manual tmux/TUI checks), and screenshots/recordings for UI changes.

### Definition of done

Before requesting review, every PR must clear:

1. **`cargo fmt`, `cargo clippy`, `cargo test`** all clean (`--features serve` if the change touches the web dashboard or structured view).
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

To add one:
1. Create `src/migrations/vNNN_description.rs` with a `pub fn run() -> anyhow::Result<()>`.
2. In `src/migrations/mod.rs`: add `mod vNNN_description;`, bump `CURRENT_VERSION`, append a `Migration { version: NNN, name: "description", run: vNNN_description::run }` entry.

Migrations must be idempotent, use `tracing::info!`, gate platform-specific ones with `#[cfg(target_os = "...")]`, and be tested by hand-crafting the old state.

`docs/cli/reference.md` is auto-generated by `cargo xtask gen-docs`; edit the clap help in `src/cli/` and re-run instead. CI enforces it.

## Website & Documentation

The public website (agent-of-empires.com) is an Astro static site in `website/`.

- **`docs/`** is the canonical source for all documentation and guide content. Edit docs here, never on the website side.
- Astro component pages (`*.astro`) like `website/src/pages/guides/index.astro` are not generated; edit them directly.

**Adding a new page to the website:**
1. Create the page in `docs/` (with a `# Title` as the first line).
2. Add an entry to the `PAGES` array in `website/scripts/sync-docs.mjs` with `source`, `dest`, `title`, and `description`.
3. Add the page's source path → website URL mapping to `URL_MAP` in the same script.
4. Add a nav entry in `website/src/data/docsNav.ts`.

The CI workflow (`.github/workflows/docs.yml`) triggers on changes to `docs/**`, `website/**`, and other relevant paths.

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
