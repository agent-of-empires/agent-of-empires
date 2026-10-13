# Adding a data migration

Breaking changes to stored data (file locations, config schema) go through `src/migrations/`, not inline fallback/compat shims. A `.schema_version` file tracks state; `migrations::run_migrations_with()` runs pending ones in order on startup and bumps the version.

A migration whose work is large and per-session may leave rows pending rather than doing all of it at startup. v027 and v033 are the examples: they stamp the version at first upgrade, then moves each session's store when that session next needs a container, with `aoe migrate` as the bulk path. If you write one of these, the startup pass must be cheap and the deferred work must have a trigger a user reaches without knowing it exists.

To add one:

1. Create `src/migrations/vNNN_description.rs` with a `pub fn run() -> anyhow::Result<()>`.
2. In `src/migrations/mod.rs`: add `mod vNNN_description;`, bump `CURRENT_VERSION`, and append a `Migration { version: NNN, name: "description", run: vNNN_description::run }` entry to `MIGRATIONS`.

Migrations must be idempotent, use `tracing::info!`, gate platform-specific ones with `#[cfg(target_os = "...")]`, and be tested by hand-crafting the old state.

Do not reuse a published migration number for a different transformation. When integrating divergent schema histories, preserve the published meaning and add idempotent reconciliation steps newer than both histories. Test upgrades from each history and a second run over the migrated state.

Published schemas 35, 36, and 37 retain custom-sort admission, creation-count
seeding, and trash identity respectively. Launch-policy conversion runs at 39;
schema 44 reconciles policy for earlier PR installations through schema 43.
That reconciliation does not initialize purge ownership: a missing journal must
remain missing rather than fabricate ownership proof.

Schema 45 records exact runner, terminal-launch and purge scope ownership. Older
records are migrated to unresolved authority, never assumed unscoped or
authorized by a SID wildcard. Unknown scope ownership retains rows, journals
and resources. Terminal launch records exact units under its existing generation
reservation before pane birth, including when later configuration disables scopes.

A migration that retypes a persisted field must call `crate::session::backup_before_migration` before rewriting, so the build that predates the new shape still has bytes it can read. What that build does with a row it cannot decode, and what the migration backup is worth to a user who forces a downgrade, is covered under [Downgrading](../installation.md#downgrading).
