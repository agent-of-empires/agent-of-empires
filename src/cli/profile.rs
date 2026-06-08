//! `agent-of-empires profile` subcommands implementation

use anyhow::{bail, Result};
use clap::Subcommand;
use std::io::{self, Write};

use crate::session;

// Note: serde_json and dirs crates used for manifest handling; ensure they're in Cargo.toml

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

    /// Lock a profile to prevent account binding changes
    Lock {
        /// Profile name, or --all for all profiles
        name: Option<String>,
        /// Lock all profiles
        #[arg(long)]
        all: bool,
    },

    /// Unlock a profile
    Unlock {
        /// Profile name
        name: String,
    },

    /// Show profile lock status and bound account
    Status,
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub async fn run(command: Option<ProfileCommands>) -> Result<()> {
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
        Some(ProfileCommands::Lock { name, all }) => {
            if all {
                lock_all_profiles().await
            } else if let Some(n) = name {
                lock_profile(&n).await
            } else {
                bail!("profile lock requires a name or --all")
            }
        }
        Some(ProfileCommands::Unlock { name }) => unlock_profile(&name).await,
        Some(ProfileCommands::Status) => show_profile_status().await,
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

async fn lock_profile(name: &str) -> Result<()> {
    // Read manifest, validate profile exists, update lock status
    let manifest = load_profile_lock_manifest()?;
    if !manifest.bindings.contains_key(name) {
        bail!("Profile '{}' not found in manifest", name);
    }
    update_profile_lock(name, true)?;
    println!("✓ Locked profile: {}", name);
    Ok(())
}

async fn lock_all_profiles() -> Result<()> {
    // Read manifest and lock all profiles
    let manifest = load_profile_lock_manifest()?;
    for profile in manifest.bindings.keys() {
        update_profile_lock(profile, true)?;
    }
    println!("✓ Locked all profiles");
    Ok(())
}

async fn unlock_profile(name: &str) -> Result<()> {
    // Unlock a specific profile
    let manifest = load_profile_lock_manifest()?;
    if !manifest.bindings.contains_key(name) {
        bail!("Profile '{}' not found in manifest", name);
    }
    update_profile_lock(name, false)?;
    println!("✓ Unlocked profile: {}", name);
    Ok(())
}

async fn show_profile_status() -> Result<()> {
    // Show lock status for all profiles with their bound accounts
    let manifest = load_profile_lock_manifest()?;
    println!(
        "Profile Lock Status (manifest: locked = {}):",
        manifest.locked
    );
    println!();
    for (name, binding) in &manifest.bindings {
        let locked_indicator = if binding.locked { "🔒" } else { "🔓" };
        let collision_marker = if binding.collision.is_some() {
            " ⚠️"
        } else {
            ""
        };
        println!(
            "  {} {:<20} account={}{}",
            locked_indicator, name, binding.account, collision_marker
        );
    }
    Ok(())
}

// Helper: load profile-lock.json manifest
fn load_profile_lock_manifest() -> Result<ProfileLockManifest> {
    let manifest_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".claude-accounts/profile-lock.json");

    if !manifest_path.exists() {
        bail!("Profile lock manifest not found: {:?}", manifest_path);
    }

    let content = std::fs::read_to_string(&manifest_path)?;
    let manifest: ProfileLockManifest = serde_json::from_str(&content)?;
    Ok(manifest)
}

// Helper: update profile lock status in manifest
fn update_profile_lock(profile: &str, locked: bool) -> Result<()> {
    let manifest_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".claude-accounts/profile-lock.json");

    let mut manifest = load_profile_lock_manifest()?;
    if let Some(binding) = manifest.bindings.get_mut(profile) {
        binding.locked = locked;
    }

    let content = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, content)?;
    Ok(())
}

// Manifest types (mirror from cx-scripts spec)
#[derive(serde::Deserialize, serde::Serialize)]
struct ProfileLockManifest {
    version: u32,
    locked: bool,
    bindings: std::collections::HashMap<String, ProfileBinding>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ProfileBinding {
    account: String,
    billing: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    collision: Option<String>,
    #[serde(default)]
    locked: bool,
}
