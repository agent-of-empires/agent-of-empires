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
        /// Profile name
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
        /// New profile name
        new_name: String,
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
    let profiles = session::list_profiles()?;

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
    // Verify profile exists
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
    let config = session::profile_config::resolve_config(&effective_profile)?;
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
