# Adding a data migration

Breaking changes to stored data (file locations, config schema) go through
`src/migrations/`, not inline fallback/compat shims. A `.schema_version` file
tracks state; `migrations::run_migrations()` runs pending ones in order on
startup and bumps the version.

To add one:

1. Create `src/migrations/vNNN_description.rs` with a `pub fn run() -> anyhow::Result<()>`.
2. In `src/migrations/mod.rs`: add `mod vNNN_description;`, bump `CURRENT_VERSION`, append a `Migration { version: NNN, name: "description", run: vNNN_description::run }` entry.

Migrations must be idempotent, use `tracing::info!`, gate platform-specific ones
with `#[cfg(target_os = "...")]`, and be tested by hand-crafting the old state.
