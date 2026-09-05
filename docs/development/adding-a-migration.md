# Adding a data migration

Breaking changes to stored data (file locations, config schema) go through
`src/migrations/`, not inline fallback/compat shims. A `.schema_version` file
tracks state; `migrations::run_migrations()` runs pending ones in order on
startup and bumps the version.

A migration whose work is large and per-session may leave rows pending rather
than doing all of it at startup. v027 is the example: it stamps the version at
first upgrade, then moves each session's store when that session next needs a
container, with `aoe migrate` as the bulk path. If you write one of these, the
startup pass must be cheap and the deferred work must have a trigger a user
reaches without knowing it exists.

To add one:

1. Create `src/migrations/vNNN_description.rs` with a `pub fn run() -> anyhow::Result<()>`.
2. In `src/migrations/mod.rs`: add `mod vNNN_description;`, bump `CURRENT_VERSION`, append a `Migration { version: NNN, name: "description", run: vNNN_description::run }` entry.

Migrations must be idempotent, use `tracing::info!`, gate platform-specific ones
with `#[cfg(target_os = "...")]`, and be tested by hand-crafting the old state.
