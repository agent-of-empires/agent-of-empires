//! CLI commands for inspecting resolved settings.

use anyhow::{bail, Result};
use clap::Subcommand;

use crate::session::settings_schema::{resolve, SettingSource};

#[derive(Subcommand)]
pub enum SettingsCommands {
    /// Explain where a setting's effective value comes from. KEY is a core
    /// `section.field` (e.g. `acp.default_agent`) or a plugin
    /// `plugin:<id>.<field>` (e.g. `plugin:acme.kit.retries`).
    Explain {
        /// The setting key to explain.
        key: String,
    },
}

pub fn run(command: SettingsCommands) -> Result<()> {
    match command {
        SettingsCommands::Explain { key } => run_explain(&key),
    }
}

fn source_label(source: &SettingSource) -> String {
    match source {
        SettingSource::User => "user value".to_string(),
        SettingSource::PluginDefault { plugin } => {
            format!("plugin default ({plugin}, declared, not yet applied at runtime)")
        }
        SettingSource::ManifestDefault { plugin } => format!("manifest default ({plugin})"),
        SettingSource::SchemaDefault => "schema default".to_string(),
    }
}

fn run_explain(key: &str) -> Result<()> {
    let Some(resolved) = resolve(key) else {
        bail!("'{key}' is not a known setting. Use a core `section.field` or a `plugin:<id>.<field>` key.");
    };
    let value = serde_json::to_string(&resolved.value).unwrap_or_else(|_| "null".to_string());
    println!("{key} = {value}");
    println!("  source: {}", source_label(&resolved.source));
    println!("  candidates (highest precedence first):");
    for c in &resolved.candidates {
        let v = serde_json::to_string(&c.value).unwrap_or_else(|_| "null".to_string());
        println!("    - {}: {v}", source_label(&c.source));
    }
    Ok(())
}
