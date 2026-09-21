# Adding a Setting

Settings are single-source: a field is declared once on its `Config` sub-struct, and the TUI, web dashboard, server validation, profile and repo overrides, and `config.toml` all derive from that declaration. In the common case, adding one is a single edit.

## The one-edit case

Add the field to the relevant `#[derive(SettingsSection)]` struct (in `src/session/config/mod.rs`, `src/sound/config.rs`, or `src/status_hooks.rs`) with a doc comment and a `#[setting(...)]` annotation:

```rust
/// Doc comment becomes the field's description on every surface.
#[serde(default)]
#[setting(label = "My Setting", widget = "toggle")]
pub my_setting: bool,
```

The `SettingsSection` derive turns it into a `FieldDescriptor` in `settings_schema::schema()`, and from there the TUI builds its row (`src/tui/settings/fields.rs`), the web renders the matching control from `GET /api/settings/schema`, the server validates PATCH leaves against the field's `web_write` policy and `validation` rule, profile and repo overrides merge generically, and `config.toml` round-trips through serde. Run `cargo test` and `cargo build --features web`; the field is live everywhere.

## Section and widget

The section comes from the struct's `#[setting_section(name = "...", category = "...")]`: `name` is the `[section]` table in `config.toml`, `category` the TUI tab, and an optional `repo_default = "allow" | "deny"` sets the repo-config policy its fields inherit.

| Widget | Backing type | Control |
|--------|--------------|---------|
| `toggle` | `bool` | switch |
| `text` | `String` | text input (`multiline` / `mono` flags) |
| `optional_text` | `Option<String>` | text input that clears to unset |
| `number` | integer | number input (`min` / `max`) |
| `slider` | integer | slider (`min` / `max` / `step`) |
| `select` | string enum | dropdown (`options = "value:Label,..."`) |
| `list` | `Vec<String>` | add/remove list |
| `custom:<id>` | anything | a bespoke control, see below |

## Attributes

Beyond `label`, `desc` (defaults to the doc comment), `widget`, `options`, `min` / `max` / `step`, and `multiline` / `mono`:

- `validate`: the server-authoritative check (`range:MIN[:MAX]`, `nonempty`, `memory_limit`, `volume_list`, `env_list`, `port_mapping_list`, `capability_list`, `security_opt_list`, `network`). If none fits, add a `ValidationKind` variant and a `validate=` keyword; that one rule drives both the client UX validator and the server gate.
- `web`: `elevation:<reason>` (passphrase step-up to save from the web) or `local_only:<reason>` (a host-execution surface the server rejects and the dashboard never renders, such as a binary path or command argv). Omit for a plain allow.
- `repo`: `allow` or `deny`, defaulting to the section's `repo_default`. Global-only fields are never repo-settable.
- `category` overrides the section's TUI tab, `advanced` groups the field under an Advanced fold, `global_only` shows it but makes it non-overridable per profile, and `skip` excludes it from the schema entirely.

## Custom widgets

For a field with no flat representation (a tagged enum, a float, a nested map), use `widget = "custom:<id>"` and register the id on **both** surfaces: `custom_value_from_json` / `custom_value_to_json` (plus the `validate()` and edit paths if needed) in `src/tui/settings/fields.rs`, and a component in `web/src/components/settings/customWidgets.tsx` wired into `customWidgetRegistry.ts`. An unregistered web id renders a visible "no control" placeholder rather than dropping the field silently.

Existing examples: `theme-name` (dynamic select plus repaint), `sound-volume` (a float slider), `logging-targets` (a per-target matrix), and `acp-defaults` (a validated JSON-object editor). For a cross-surface side effect after a save, pass `onAfterSave` to the web `SchemaSection`.

## What stays out of the schema

`#[setting(skip)]` is for fields that are not user-facing settings. Some things are deliberately unschematized:

- **`hooks`** has no `SettingsSection` at all. Hooks are arbitrary commands, so the hard exclusion is defense in depth against a future policy change making them web-writable.
- **`Config.environment`** is a root-level `Vec<String>` with no section, so it is TUI and `config.toml` only. Surfacing it would need a config-layout migration.
- **`diff`** is schema-backed for the TUI, but the web Diff tab is intentionally client-local.
- **`telemetry`** is in the schema, but the web toggle uses a dedicated consent endpoint that records "has responded" and honors `DO_NOT_TRACK`.
- **`app_state`** is global-only runtime bookkeeping persisted to `state.toml` (see the [configuration reference](../guides/configuration.md#file-locations)). Read and write it through `update_app_state` / `AppStateConfig::load`, never `update_config`, which writes `config.toml` and strips `app_state` on save. Before adding a field shaped like this, reconsider whether it is a setting at all.

A plugin declares its own settings in its `aoe-plugin.toml` instead, and the host turns each into a virtual `plugin:<id>` schema section that renders and validates through the same path.

Renaming or relocating a stored field is a breaking change to `config.toml`, so route it through a migration in `src/migrations/` rather than an inline fallback.

## Tests

The schema, server policy, and validators have unit tests under `src/session/config/settings_schema/`. A custom widget needs a TUI round-trip test and a web contract test (`web/src/components/settings/__tests__/customWidgets.test.tsx`). A user-facing dashboard settings flow must also update `web/tests/coverage-matrix.json` and add or extend the matching test; see `web/AGENTS.md`.
