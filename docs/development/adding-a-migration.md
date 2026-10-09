# Adding a data migration

Breaking changes to stored data (file locations, config schema) go through `src/migrations/`, not inline fallback/compat shims. A `.schema_version` file tracks state; `migrations::run_migrations_with()` runs pending ones in order on startup and bumps the version.

A migration whose work is large and per-session may leave rows pending rather than doing all of it at startup. v027 and v033 are the examples: they stamp the version at first upgrade, then moves each session's store when that session next needs a container, with `aoe migrate` as the bulk path. If you write one of these, the startup pass must be cheap and the deferred work must have a trigger a user reaches without knowing it exists.

To add one:

1. Create `src/migrations/vNNN_description.rs` with an anchored `run(&AnchoredDir, version) -> anyhow::Result<()>`. Keep the original application and participating profile descriptors through publication.
2. In `src/migrations/mod.rs`: add its module, bump `CURRENT_VERSION`, and append `(NNN, "description", MigrationAction::Anchored(vNNN_description::run))`. The anchored action publishes its schema marker only after its data barriers succeed. Existing `Legacy` actions retain their released transformation signatures.

Migrations must be idempotent, use `tracing::info!`, gate platform-specific ones with `#[cfg(target_os = "...")]`, and be tested by hand-crafting the old state.

A migration that retypes a persisted field must preserve an exact, immutable preimage before rewriting. See `v042_canonical_execution_journal` for descriptor-relative backup, publication, physical alias handling and directory barriers, including unchanged stores on retry. An unreadable or malformed schema marker is an error, not version zero; a missing required retention ledger must not be regenerated. A current-version retry must sync the original application directory before deferred reconcilers run, even when the schema marker is already visible. See [Downgrading](../installation.md#downgrading) for what backups permit.

Multi-profile writers preserve the workspace, identity, namespace/lifecycle, then storage lock hierarchy. Transition fences use every original logical application namespace before physical profile aliases are coalesced. Each same-role flock cohort ranks the actual opened lock files by `(st_dev, st_ino)` and retains them throughout the operation. Canonical or lexical profile paths cannot establish a cross-process lock order. In-process save mutexes independently follow their shared `Arc` address order.
