# Adding a Setting

Settings in AoE are single-source (#1692): a field is declared once on its
`Config` sub-struct, and every surface (TUI, web dashboard, server validation,
profile/repo overrides, `config.toml`) derives from that one declaration. In
the common case, adding a setting is a single edit.

## The one-edit case

Add the field to the relevant `#[derive(SettingsSection)]` struct (in
`src/session/config.rs`, `src/sound/config.rs`, or `src/status_hooks.rs`) with a
doc comment and a `#[setting(...)]` annotation:

```rust
/// Doc comment becomes the field's description on every surface.
#[serde(default)]
#[setting(label = "My Setting", widget = "toggle")]
pub my_setting: bool,
```

That is the whole change. The `SettingsSection` derive (the `aoe-settings-derive`
crate) turns the annotated field into a `FieldDescriptor` in
`settings_schema::schema()`, and from there:

- **TUI** builds its row from the schema (`src/tui/settings/fields.rs`). No
  `FieldKey` or `build_*` to touch.
- **Web** fetches `GET /api/settings/schema` and renders the matching control
  (`web/src/components/settings/SchemaSection.tsx`). The field appears on its
  tab automatically.
- **Server** validates the PATCH leaf against the field's `web_write` policy and
  `validation` rule (`settings_schema::validate_patch`).
- **Profile / repo overrides** are sparse JSON merged generically; nothing to
  extend.
- **`config.toml`** round-trips via `serde`.

Run `cargo test` and `cargo build`; the field is live on all
surfaces.

## Choosing the section and widget

The section is the struct's `#[setting_section(name = "...", category = "...")]`.
`name` is the `[section]` table in `config.toml`; `category` is the TUI tab.

Pick a `widget` for the field's type:

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

## Attribute reference

`label`, `desc` (defaults to the doc comment), `widget`, `options` (for
`select`), `min` / `max` / `step`, `multiline` / `mono`, plus:

- `validate`: server-authoritative value check (`range:MIN[:MAX]`, `nonempty`,
  `memory_limit`, `volume_list`, `env_list`, `port_mapping_list`). Add a new
  `ValidationKind` variant (`src/session/settings_schema/`) and a `validate=`
  keyword (`aoe-settings-derive`) if none fits; that is what drives both the
  client UX validator and the server gate from one rule.
- `web`: `elevation:<reason>` (passphrase step-up required to save from the
  web) or `local_only:<reason>` (host-execution surface the server rejects and
  the dashboard never renders, e.g. a binary path or command argv). Omit for a
  plain allow.
- `category`: override the section's default TUI tab.
- `advanced`: group the field under an "Advanced" fold on both surfaces.
- `global_only`: shown but not profile-overridable (the dashboard adds an
  "applies to all profiles" hint).
- `skip`: exclude the field from the schema entirely (rare; see below).

## Custom widgets

When a field has no flat representation (a tagged enum, a float, a nested map),
use `widget = "custom:<id>"` and register the id on **both** surfaces:

- **TUI**: `custom_value_from_json` / `custom_value_to_json` (and, if it needs
  validation or a multiline editor, the `validate()` and edit paths) in
  `src/tui/settings/fields.rs`.
- **Web**: a component in `web/src/components/settings/customWidgets.tsx`, wired
  into `web/src/components/settings/customWidgetRegistry.ts`.

An unregistered web id renders a visible "no control" placeholder rather than
silently dropping the field, so a half-done custom widget is obvious.

Existing examples: `theme-name` (dynamic select + repaint), `sound-mode` (a
`random` / `{specific}` enum), `sound-volume` (a float slider), `logging-targets`
(a per-target matrix), and `acp-defaults` (a JSON-object editor, validated so a
malformed edit is rejected rather than wiping the map).

For a cross-surface side-effect after a save (not part of the value itself),
pass `onAfterSave` to the web `SchemaSection`; the acp section uses it to refresh
the `serverAbout` snapshot that tool cards read live.

## When to use `skip`, and what stays out of the schema

`#[setting(skip)]` keeps a field off every surface. Use it only for fields that
are not user-facing settings. A few things are deliberately not schematized:

- **`hooks`** (`HooksConfig`) has no `SettingsSection` at all. Hooks are
  arbitrary commands (an RCE surface); the hard exclusion is defense-in-depth so
  a future policy change cannot make them web-writable by accident. Leave it
  out.
- **`Config.environment`** (the host environment list) is a root-level
  `Vec<String>` with no section, so it is TUI / `config.toml` only. Surfacing it
  would need a breaking config-layout migration (move it under a section).
- **`diff`** is schema-backed for the TUI, but the web Diff tab is intentionally
  client-local (`localStorage`), so it does not round-trip through the schema.
- **`telemetry`** is in the schema, but the web toggle uses a dedicated consent
  endpoint (it records "has responded" and honors `DO_NOT_TRACK`), not the
  generic PATCH.

## Breaking changes

Renaming or relocating a stored field is a breaking change to `config.toml`;
route it through a migration in `src/migrations/` (see
[the migrations section in AGENTS.md](../../AGENTS.md)), not an inline
fallback.

## Tests

- The schema, server policy, and validators have unit tests under
  `src/session/settings_schema/`.
- A custom widget should have a TUI round-trip test
  (`src/tui/settings/fields.rs`) and a web contract test
  (`web/src/components/settings/__tests__/customWidgets.test.tsx`).
- A user-facing dashboard settings flow must update
  `web/tests/coverage-matrix.json` and add or extend the appropriate Vitest /
  Playwright test (see [Playwright + Vitest testing](playwright.md)).
