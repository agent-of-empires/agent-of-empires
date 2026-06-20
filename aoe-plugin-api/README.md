# aoe-plugin-api

The stable types a plugin author (and the in-tree host) compiles against for
the [Agent of Empires](https://github.com/agent-of-empires/agent-of-empires)
plugin system: the `aoe-plugin.toml` manifest schema (`PluginManifest` and its
contribution types), the `Capability` set, and the `PluginId` newtype.

Plugins do not depend on this crate to run; a worker speaks newline-delimited
JSON-RPC over stdio in any language. This crate is the host-side schema and the
reference for what a manifest may declare.

See `docs/development/writing-plugins.md` in the main repository for the
authoring guide, and `docs/development/internals/plugin-system.md` for the
architecture and security model.

## Compatibility

The manifest carries an `api_version`; the host rejects a manifest targeting a
newer version than it supports. The public enums and structs are
`#[non_exhaustive]`, so adding a variant or field is not a breaking change for
downstream Rust consumers.

## License

MIT.
