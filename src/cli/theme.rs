//! CLI commands for managing themes

use anyhow::{bail, Result};
use clap::Subcommand;

use crate::tui::styles::{
    available_themes, builtin_theme_names, custom_themes_dir, export_theme_toml, is_builtin_theme,
    load_theme,
};

#[derive(Subcommand)]
pub enum ThemeCommands {
    /// List all available themes (built-in and custom)
    #[command(alias = "ls")]
    List,

    /// Export a built-in theme as a TOML file for customization
    Export {
        /// Theme name to export
        name: String,

        /// Output file path (defaults to `<name>.toml` in the themes directory)
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Show the custom themes directory path
    Dir,
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub fn run_list() {
    let themes = available_themes();
    let builtin_count = builtin_theme_names().count();

    println!("Built-in themes:");
    for name in builtin_theme_names() {
        println!("  {}", name);
    }

    let custom: Vec<_> = themes
        .iter()
        .filter(|name| !is_builtin_theme(name))
        .collect();
    if !custom.is_empty() {
        println!("\nCustom themes:");
        for name in &custom {
            println!("  {}", name);
        }
    }

    println!("\n{} built-in, {} custom", builtin_count, custom.len());
}

#[tracing::instrument(target = "cli.session", skip_all, fields(name = %name))]
pub fn run_export(name: &str, output: Option<&str>) -> Result<()> {
    let all = available_themes();
    if !all.iter().any(|t| t == name) {
        bail!(
            "Unknown theme '{}'. Run `aoe theme list` to see available themes.",
            name
        );
    }

    let theme = load_theme(name);
    let toml_str = export_theme_toml(&theme)?;

    match output {
        Some(path) => {
            std::fs::write(path, &toml_str)?;
            println!("Exported '{}' to {}", name, path);
        }
        None => {
            let dir = custom_themes_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine themes directory"))?;
            std::fs::create_dir_all(&dir)?;

            let filename = if is_builtin_theme(name) {
                format!("custom-{}.toml", name)
            } else {
                format!("{}.toml", name)
            };
            let path = dir.join(&filename);
            std::fs::write(&path, &toml_str)?;
            println!("Exported '{}' to {}", name, path.display());
            if let Some(stem) = path.file_stem() {
                println!(
                    "Edit the file and it will appear as '{}' in the theme selector.",
                    stem.to_string_lossy()
                );
            }
        }
    }

    Ok(())
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub fn run_dir() -> Result<()> {
    match custom_themes_dir() {
        Some(dir) => {
            println!("{}", dir.display());
            Ok(())
        }
        None => bail!("Cannot determine themes directory"),
    }
}
