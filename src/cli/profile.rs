//! `agent-of-empires profile` subcommands implementation

use anyhow::{bail, Result};
use clap::Subcommand;
use std::io::{self, Write};

use crate::session;

#[derive(Subcommand)]
pub enum ProfileCommands {
    /// List all profiles
    #[command(alias = "ls")]
    List,

    /// Create a new profile
    #[command(alias = "new")]
    Create {
        /// Profile name: letters, digits, `_` and `-` only, at most 64
        /// characters; `all` is reserved
        name: String,
    },

    /// Delete a profile
    #[command(alias = "rm")]
    Delete {
        /// Profile name
        name: String,
    },

    /// Rename a profile
    #[command(alias = "mv")]
    Rename {
        /// Current profile name
        old_name: String,
        /// New profile name (same rules as `aoe profile create`)
        new_name: String,
    },

    /// Preserve an invalid rename checkpoint after reviewing its digest
    RepairRenameCheckpoint {
        /// Active checkpoint filename printed by startup recovery
        #[arg(long)]
        checkpoint: String,
        /// SHA-256 printed by startup recovery
        #[arg(long)]
        sha256: String,
        /// Acknowledge the checkpoint's old profile when its endpoints are identifiable
        #[arg(long, requires = "new_profile")]
        old_profile: Option<String>,
        /// Acknowledge the checkpoint's new profile when its endpoints are identifiable
        #[arg(long, requires = "old_profile")]
        new_profile: Option<String>,
    },

    /// Show or set default profile
    Default {
        /// Profile name (optional, shows current if not provided)
        name: Option<String>,
    },

    /// Show profile-derived values for scripts
    Show {
        /// Print the resolved status map for an agent
        #[arg(long = "status-map", value_name = "AGENT")]
        status_map: Option<String>,

        /// Emit JSON output
        #[arg(long)]
        json: bool,
    },
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub async fn run(profile: &str, command: Option<ProfileCommands>) -> Result<()> {
    match command {
        Some(ProfileCommands::List) | None => list_profiles().await,
        Some(ProfileCommands::Create { name }) => create_profile(&name).await,
        Some(ProfileCommands::Delete { name }) => delete_profile(&name).await,
        Some(ProfileCommands::Rename { old_name, new_name }) => {
            rename_profile(&old_name, &new_name).await
        }
        Some(ProfileCommands::RepairRenameCheckpoint {
            checkpoint,
            sha256,
            old_profile,
            new_profile,
        }) => {
            let expected_profiles = old_profile.as_deref().zip(new_profile.as_deref());
            let preserved = session::profile_rename::repair_rename_checkpoint(
                &checkpoint,
                &sha256,
                expected_profiles,
            )?;
            println!("Preserved checkpoint evidence at {}", preserved.display());
            Ok(())
        }
        Some(ProfileCommands::Default { name }) => {
            if let Some(n) = name {
                set_default_profile(&n).await
            } else {
                show_default_profile().await
            }
        }
        Some(ProfileCommands::Show { status_map, json }) => {
            show_profile_value(profile, status_map.as_deref(), json).await
        }
    }
}

async fn list_profiles() -> Result<()> {
    let profiles = session::list_profiles_for_display()?;

    if profiles.is_empty() {
        println!("No profiles found.");
        println!("Run 'aoe' to create the first profile automatically.");
        return Ok(());
    }

    let default_profile = session::config::resolve_default_profile();

    println!("Profiles:");
    for p in &profiles {
        if *p == default_profile {
            println!("  * {} (default)", p);
        } else {
            println!("    {}", p);
        }
    }
    println!("\nTotal: {} profiles", profiles.len());

    Ok(())
}

async fn create_profile(name: &str) -> Result<()> {
    session::create_profile(name)?;
    println!("✓ Created profile: {}", name);
    println!("  Use with: aoe -p {}", name);
    Ok(())
}

async fn rename_profile(old_name: &str, new_name: &str) -> Result<()> {
    session::rename_profile(old_name, new_name)?;
    println!("✓ Renamed profile: {} -> {}", old_name, new_name);
    Ok(())
}

async fn delete_profile(name: &str) -> Result<()> {
    print!(
        "Are you sure you want to delete profile '{}'? This will remove all sessions in this profile. [y/N] ",
        name
    );
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    if response.trim().to_lowercase() != "y" {
        println!("Cancelled.");
        return Ok(());
    }

    session::delete_profile(name)?;
    println!("✓ Deleted profile: {}", name);
    Ok(())
}

async fn show_default_profile() -> Result<()> {
    println!(
        "Default profile: {}",
        session::config::resolve_default_profile()
    );
    Ok(())
}

async fn set_default_profile(name: &str) -> Result<()> {
    let profiles = session::list_profiles()?;
    if !profiles.contains(&name.to_string()) {
        bail!("Profile '{}' does not exist", name);
    }

    session::set_default_profile(name)?;
    println!("✓ Default profile set to: {}", name);
    Ok(())
}

async fn show_profile_value(profile: &str, status_map: Option<&str>, json: bool) -> Result<()> {
    let Some(agent) = status_map else {
        bail!("nothing to show; pass --status-map <agent>");
    };
    let effective_profile = session::resolve_existing_profile(profile)?;
    let config = session::config::profile_config::resolve_config(&effective_profile)?;
    let map = crate::agents::effective_status_map(&config, agent)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        for (event, status) in map {
            println!("{} = {}", event, status.as_str());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse_profile_cli(args: &[&str]) -> std::result::Result<crate::cli::Cli, clap::Error> {
        crate::cli::Cli::try_parse_from(std::iter::once("aoe").chain(args.iter().copied()))
    }

    async fn run_profile_cli(args: &[&str]) -> Result<()> {
        let cli = parse_profile_cli(args)?;
        let profile = cli.profile.as_deref().unwrap_or("default");
        let Some(crate::cli::Commands::Profile { command }) = cli.command else {
            bail!("expected profile command");
        };
        run(profile, command).await
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn repair_rename_checkpoint_cli_parses_pairs_and_preserves_verified_evidence() {
        use sha2::{Digest, Sha256};

        let _home = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();
        let checkpoint = ".profile-rename.json";
        let active = app_dir.join(checkpoint);
        let raw = br#"{"version":1,"old":"before","new":"after","session_ids":"invalid"}"#;
        std::fs::write(&active, raw).unwrap();
        let digest = format!("{:x}", Sha256::digest(raw));

        let common = [
            "profile",
            "repair-rename-checkpoint",
            "--checkpoint",
            checkpoint,
            "--sha256",
            digest.as_str(),
        ];
        let old_only = [
            "profile",
            "repair-rename-checkpoint",
            "--checkpoint",
            checkpoint,
            "--sha256",
            digest.as_str(),
            "--old-profile",
            "before",
        ];
        let new_only = [
            "profile",
            "repair-rename-checkpoint",
            "--checkpoint",
            checkpoint,
            "--sha256",
            digest.as_str(),
            "--new-profile",
            "after",
        ];
        assert!(parse_profile_cli(&old_only).is_err());
        assert!(parse_profile_cli(&new_only).is_err());

        let error = run_profile_cli(&common)
            .await
            .expect_err("identifiable endpoints require an explicit acknowledgement pair");
        assert!(error
            .to_string()
            .contains("provide --old-profile and --new-profile"));
        assert_eq!(std::fs::read(&active).unwrap(), raw);

        let mismatch = [
            "profile",
            "repair-rename-checkpoint",
            "--checkpoint",
            checkpoint,
            "--sha256",
            digest.as_str(),
            "--old-profile",
            "wrong",
            "--new-profile",
            "after",
        ];
        let error = run_profile_cli(&mismatch)
            .await
            .expect_err("mismatched profile acknowledgement must be rejected");
        assert!(error
            .to_string()
            .contains("checkpoint endpoints do not match"));
        assert_eq!(std::fs::read(&active).unwrap(), raw);

        let bad_digest = [
            "profile",
            "repair-rename-checkpoint",
            "--checkpoint",
            checkpoint,
            "--sha256",
            "not-a-sha256",
            "--old-profile",
            "before",
            "--new-profile",
            "after",
        ];
        let error = run_profile_cli(&bad_digest)
            .await
            .expect_err("invalid digest must be rejected");
        assert!(error.to_string().contains("64 hexadecimal characters"));
        assert_eq!(std::fs::read(&active).unwrap(), raw);

        let acknowledged = [
            "profile",
            "repair-rename-checkpoint",
            "--checkpoint",
            checkpoint,
            "--sha256",
            digest.as_str(),
            "--old-profile",
            "before",
            "--new-profile",
            "after",
        ];
        run_profile_cli(&acknowledged)
            .await
            .expect("matching acknowledgement and digest should preserve the checkpoint");

        let preserved = app_dir.join(format!("{checkpoint}.corrupt-{digest}"));
        assert!(!active.exists());
        assert_eq!(std::fs::read(preserved).unwrap(), raw);
    }
}
