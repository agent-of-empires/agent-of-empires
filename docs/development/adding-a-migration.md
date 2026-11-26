# Adding a data migration

Breaking changes to stored data (file locations, config schema) go through `src/migrations/`, not inline fallback/compat shims. A `.schema_version` file tracks state; `migrations::run_migrations_with()` runs pending ones in order on startup and bumps the version.

A migration whose work is large and per-session may leave rows pending rather than doing all of it at startup. v027 and v033 are the examples: they stamp the version at first upgrade, then moves each session's store when that session next needs a container, with `aoe migrate` as the bulk path. If you write one of these, the startup pass must be cheap and the deferred work must have a trigger a user reaches without knowing it exists.

A bare, nonmoving sandbox bootstrap that encounters a named profile's initial JSON/container/row-identity inventory failure must defer the whole physical checkpoint before planning. Never continue with partial inventory, copy or retype data, retire resources, or stamp per-row store generations as complete. A successful deferral may still advance the overall lazy migration schema marker; that marker is not physical completion proof.

This exception does not cover scoped transitions, recovery/reclamation scans, global registries, ownership proofs, journals or filesystem I/O. Those paths remain strict. Deferral does not latch native health: repaired data may restore health, but health recovery alone never proves physical retirement; the strict inventory and ownership checks must still succeed.

To add one:

1. Create `src/migrations/vNNN_description.rs` with a `pub fn run() -> anyhow::Result<()>`.
2. In `src/migrations/mod.rs`: add `mod vNNN_description;`, bump `CURRENT_VERSION`, and append `(NNN, "description", vNNN_description::run)` to `MIGRATIONS`.

Migrations must be idempotent, use `tracing::info!`, gate platform-specific ones with `#[cfg(target_os = "...")]`, and be tested by hand-crafting the old state.

Do not reuse a published migration number for a different transformation. When integrating divergent schema histories, preserve the published meaning and add idempotent reconciliation steps newer than both histories. Test upgrades from each history and a second run over the migrated state.

ACP reconciliation (v068) preserves ACP54–57 establishment, event scope,
prompt publication and budget knowledge. A main55 marker does not prove ACP55
event scope. The registry preserves main63 for retired color labels, main64 for
retired sound file overrides, main65 for separate account-limit estimate rows,
main66 for removing retired agent-update restart fields from session rows, and
main67 for dropping retired restart wake settings.
The estimate migration first reconciles the cache shape if an ACP marker skipped
main58. v068 repeats the color, sound, agent-update, and restart wake cleanups
because ACP markers do not prove those main steps ran. In particular, a marker
of 67 can mean main's wake cleanup or ACP's reconciliation, not necessarily
both. The runner stamps each migration only after it succeeds; a retry can
safely repeat completed steps.

Qualification reconciliation also validates recorded private child source and
ancestry metadata. It does not synthesize that proof from reported parent/child
IDs, rewrite historical content into current authority, or treat a current schema
marker as proof that a legacy child may join a current resource.

Adding an optional field whose missing and null forms mean the same thing does not require rewriting existing rows.

Scope and process authority must remain exact. Missing legacy proof stays unresolved until complete authoritative inventory proves absence. Preserve captured units, process births, native runtime and producer identity. An unfinished constructor prevents discharge of unknown scope debt. Never overwrite readable malformed proof or infer launch or retirement authority from a missing field. Inventory selectors never authorize retirement, except units in the pre-custody naming format, which earlier releases stopped by the same glob.

Ordinary startup performs metadata migrations, not explicit scope-authority recovery. Unreadable components retain their bytes for repair; a current schema marker does not prove physical completion. After repair, explicit `aoe migrate` retries supported metadata reconciliation and separately requires a complete, consistent session, worker and purge-journal inventory before scope-authority writes. Malformed or ambiguous inventory, live unknown units and incomplete manager or kernel proof refuse that recovery. Follow [Ownership recovery](../installation.md#ownership-recovery) rather than clearing evidence.

Runtime stop, launch and cleanup require their migrated ownership journals. A missing journal must not be recreated empty, and a historical receipt must not acquire manufactured process-birth or tmux-incarnation proof. Restore only complete original authority using the [operator workflow](../installation.md#ownership-recovery).

Managed restart, rollback and dashboard promotion reconcile supported historical launch policy under their existing lifecycle transaction. The schema compatibility guard runs before stopping the verified predecessor; future or unsupported data remains untouched. Full parent migrations then run under `serve.lock`, and the replacement child only asserts the current schema. Torn inactive historical launch records may be quarantined only under serve, lifetime and state exclusion after proving daemon absence. Preserve peer launch records and credentials.

A migration that retypes a persisted field must call `crate::session::backup_before_migration` before rewriting, so the build that predates the new shape still has bytes it can read. What that build does with a row it cannot decode, and what the migration backup is worth to a user who forces a downgrade, is covered under [Downgrading](../installation.md#downgrading).
