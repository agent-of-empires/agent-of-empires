---
name: aoe-invariants
description: Review an Agent of Empires change for cross-file invariants that a diff hunk alone does not show, such as persisted data that needs a registered migration that survives every on-disk shape, settings that must stay single-source, Session writes that must go through the Daemon, plugin API versions, debug and release namespacing, tracing targets, and docs the change makes stale. Use as one lens in a code review run.
---

# Review lens: AoE invariants

Review the companion changes an Agent of Empires change owes elsewhere in the repository. AGENTS.md is the canonical rule source; cite its section, or the linked doc, in every finding.

## Scope

- **Persisted state** Whether a renamed, removed or retyped stored field or path ships a registered migration instead of an inline fallback (docs/development/adding-a-migration.md); whether a serde default yields a value that readers treat as real data; whether a setting is declared once with `#[setting(...)]` and nowhere else (docs/development/adding-settings.md).
- **Migration safety** Whether a migration completes on every on-disk shape the storage layer accepts (a missing, dangling or symlinked file, a partly applied earlier run, records an older build wrote) without aborting startup or skipping records; whether it validates before it rewrites, keeps the guards earlier migrations had, and makes data durable before advancing the schema marker; whether an upgrade of a versioned web `localStorage` layout keeps the identities of objects still live in the Daemon.
- **Daemon, Client and plugin contracts** Whether a new Session state change or Daemon-read config write sits behind a Daemon endpoint that each Client calls, and whether a Client tells refusals apart by status and `AoE-Error-Code`, with a new `ApiErrorCode` when it needs to name one (AGENTS.md "Daemon and Clients", ADR 0004); whether a plugin-visible method or type change bumps `API_VERSION` in `aoe-plugin-api/src/lib.rs` and the docs that name the version.
- **Namespaces and logging axes** Whether new code derives the app directory, tmux socket, session prefix and dashboard port from the existing helpers, so debug and release builds stay isolated; whether each new `tracing` event in `src/` carries an explicit `target:`.
- **Docs drift** Whether the change leaves `docs/`, `CONTEXT.md` or an ADR it does not edit materially wrong, and whether a new website page has its `website/scripts/sync-docs.mjs` entry (docs/development/adding-a-website-page.md).

## Method

Start from what the diff changes, then search for the companion it owes. For a changed serde type, find where it is written to disk and look for a matching `src/migrations/vNNN_*.rs` and its registration in `src/migrations/mod.rs`. For a migration, walk it against each shape `src/session/storage.rs` and earlier migrations accept, including a symlinked file, a link whose target or target directory is missing, and a half-migrated file. For a new CLI or TUI action that mutates state, find the call path and check whether it reaches a Daemon endpoint or writes storage itself. For a changed user-facing behavior, flag, endpoint or term, search `docs/`, `CONTEXT.md` and `docs/adr/` for text that now describes the old behavior.

## Threshold

Report when the change breaks one of these invariants and you can name both the changed line and the missing or contradicting companion.

Do not report:
- Rules a single line shows (em dashes, `#[allow(dead_code)]`, OS-specific code placement, fixed sleeps in tests); the repository verifiers and `review-maintainability-and-code-quality` own them.
- Artifacts CI already regenerates or checks: `docs/cli/reference.md`, `web/src/lib/generated/settingsKeys.ts`, `DEFAULT_TARGET_ROOTS` and the logging target mirror, and `flake.nix` embedded assets.
- Secrets in logs, injection and authorization; `review-security-and-privacy` owns them.
- Claims in Markdown the change itself edits; `review-markdown` checks them against the code.
- Coverage-matrix entries and Daemon HTTP or MCP contract docs; the `web-change-updates-coverage-matrix` and `api-contract-change-updates-docs` verifiers own them.
- Owner-only file creation and paths a sandbox can write; `aoe-sandbox-boundary` owns them.
- Whether tests reach, isolate or pin the change; `aoe-test-evidence` owns them.
- Races and runtime state transitions; `aoe-runtime-lifecycle` owns them.
- Migration numbers that collide with other open pull requests; the diff cannot show them.
- Existing out-of-band Client writes the change only moves or reformats, and commands ADR 0004 keeps local.
- Additive optional fields whose missing and null forms mean the same thing.

## Reporting

- Name the changed line, the companion that is missing or now wrong, and the rule source.
- State the concrete fix: the migration file and registration to add, the on-disk shape the migration must handle, the Daemon endpoint to call or add, the helper to use, the API version to bump, or the doc passage to update.
