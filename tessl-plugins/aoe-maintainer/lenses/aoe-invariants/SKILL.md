---
name: aoe-invariants
description: Review an Agent of Empires change for cross-file invariants that a diff hunk alone does not show, such as persisted data that needs a migration, settings that must stay single-source, Session writes that must go through the Daemon, debug and release namespacing, tracing targets, and docs the change makes stale. Use as one lens in a code review run.
---

# Review lens: AoE invariants

Review the companion changes an Agent of Empires change owes elsewhere in the repository. AGENTS.md is the canonical rule source; cite its section, or the linked doc, in every finding.

## Scope

- **Persisted state** Whether a renamed, removed or retyped stored field or path ships a registered migration instead of an inline fallback (docs/development/adding-a-migration.md); whether a setting is declared once with `#[setting(...)]` and nowhere else (docs/development/adding-settings.md); whether a file holding a token, passphrase or credential in the app directory is created owner-only (mode 0o600) through the existing private-write helpers.
- **Daemon and Client boundary** Whether a new Session state change or Daemon-read config write sits behind a Daemon endpoint that each Client calls, and whether a Client tells refusals apart by status and `AoE-Error-Code`, with a new `ApiErrorCode` when it needs to name one (AGENTS.md "Daemon and Clients", ADR 0004).
- **Namespaces and logging axes** Whether new code derives the app directory, tmux socket, session prefix and dashboard port from the existing helpers, so debug and release builds stay isolated; whether each new `tracing` event in `src/` carries an explicit `target:`.
- **Docs drift** Whether the change leaves `docs/`, `CONTEXT.md` or an ADR materially wrong, and whether a new website page has its `website/scripts/sync-docs.mjs` entry (docs/development/adding-a-website-page.md).

## Method

Start from what the diff changes, then search for the companion it owes. For a changed serde type, find where it is written to disk and look for a matching `src/migrations/vNNN_*.rs` and its registration in `src/migrations/mod.rs`. For a new CLI or TUI action that mutates state, find the call path and check whether it reaches a Daemon endpoint or writes storage itself. For a changed user-facing behavior, flag, endpoint or term, search `docs/`, `CONTEXT.md` and `docs/adr/` for text that now describes the old behavior.

## Threshold

Report when the change breaks one of these invariants and you can name both the changed line and the missing or contradicting companion.

Do not report:
- Rules a single line shows (em dashes, `#[allow(dead_code)]`, OS-specific code placement, fixed sleeps in tests); the repository verifiers and `review-maintainability-and-code-quality` own them.
- Artifacts CI already regenerates or checks: `docs/cli/reference.md`, `web/src/lib/generated/settingsKeys.ts`, `DEFAULT_TARGET_ROOTS` and the logging target mirror, and `flake.nix` embedded assets.
- Secrets in logs, injection and authorization; `review-security-and-privacy` owns them.
- Existing out-of-band Client writes the change only moves or reformats, and commands ADR 0004 keeps local.
- Additive optional fields whose missing and null forms mean the same thing.

## Reporting

- Name the changed line, the companion that is missing or now wrong, and the rule source.
- State the concrete fix: the migration file and registration to add, the Daemon endpoint to call or add, the helper to use, or the doc passage to update.
