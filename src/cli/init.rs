//! `agent-of-empires init` command implementation

use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::path::PathBuf;

use crate::session::config::repo_config::INIT_TEMPLATE;

#[derive(Args)]
pub struct InitArgs {
    /// Directory to initialize (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[tracing::instrument(target = "cli.init", skip_all)]
pub async fn run(args: InitArgs) -> Result<()> {
    let path = if args.path.as_os_str() == "." {
        std::env::current_dir()?
    } else {
        if !args.path.exists() {
            bail!("Path does not exist: {}", args.path.display());
        }
        args.path
            .canonicalize()
            .with_context(|| format!("Failed to resolve path: {}", args.path.display()))?
    };

    let config_dir = path.join(".agent-of-empires");
    let config_path = config_dir.join("config.toml");

    // Check for both new and legacy paths
    let legacy_path = path.join(".aoe").join("config.toml");
    if config_path.exists() {
        bail!(
            ".agent-of-empires/config.toml already exists at {}\nEdit it directly to make changes.",
            config_path.display()
        );
    }
    if legacy_path.exists() {
        bail!(
            "Legacy .aoe/config.toml found at {}\nRename .aoe/ to .agent-of-empires/ to use the new path, or edit it directly.",
            legacy_path.display()
        );
    }

    fs::create_dir_all(&config_dir)?;
    fs::write(&config_path, INIT_TEMPLATE)?;

    println!(
        "Created .agent-of-empires/config.toml at {}",
        path.display()
    );
    println!("Edit the file to configure hooks and session defaults for this repo.");

    Ok(())
}
