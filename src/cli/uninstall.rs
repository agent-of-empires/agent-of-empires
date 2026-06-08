//! `agent-of-empires uninstall` command implementation

use anyhow::Result;
use clap::Args;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

#[derive(Args)]
pub struct UninstallArgs {
    /// Keep data directory (sessions, config, logs)
    #[arg(long)]
    keep_data: bool,

    /// Keep tmux configuration
    #[arg(long)]
    keep_tmux_config: bool,

    /// Show what would be removed without removing
    #[arg(long)]
    dry_run: bool,

    /// Skip confirmation prompts
    #[arg(short = 'y')]
    yes: bool,
}

struct FoundItem {
    item_type: String,
    path: PathBuf,
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub async fn run(args: UninstallArgs) -> Result<()> {
    println!("╔════════════════════════════════════════╗");
    println!("║     Agent of Empires Uninstaller       ║");
    println!("╚════════════════════════════════════════╝");
    println!();

    if args.dry_run {
        println!("DRY RUN MODE - Nothing will be removed");
        println!();
    }

    let home_dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;

    // Collect all possible data directory locations. The home-dotfile path is
    // always included (it is the macOS default and the pre-XDG Linux location),
    // alongside the XDG path, so either layout is cleaned up.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let data_dirs = {
        let mut dirs = vec![home_dir.join(".agent-of-empires")];
        if let Ok(base) = crate::session::xdg_config_base() {
            dirs.push(base.join("agent-of-empires"));
        }
        dirs
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let data_dirs = vec![home_dir.join(".agent-of-empires")];

    let mut found_items: Vec<FoundItem> = Vec::new();

    // Check for Homebrew installation (formula is named "aoe")
    let homebrew_installed = Command::new("brew")
        .args(["list", "aoe"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    if homebrew_installed {
        found_items.push(FoundItem {
            item_type: "homebrew".to_string(),
            path: PathBuf::new(),
        });
        println!("Found: Homebrew installation");
    }

    // Check common binary locations for both "aoe" and "agent-of-empires"
    let mut binary_locations = vec![
        home_dir.join(".local/bin/aoe"),
        PathBuf::from("/usr/local/bin/aoe"),
        home_dir.join("bin/aoe"),
        home_dir.join(".local/bin/agent-of-empires"),
        PathBuf::from("/usr/local/bin/agent-of-empires"),
        home_dir.join("bin/agent-of-empires"),
    ];

    // Also check the currently running binary's location
    if let Ok(current_exe) = std::env::current_exe() {
        if let Ok(canonical) = current_exe.canonicalize() {
            if !binary_locations.contains(&canonical) {
                binary_locations.push(canonical);
            }
        }
    }

    for loc in &binary_locations {
        if loc.exists() && !found_items.iter().any(|i| i.path == *loc) {
            found_items.push(FoundItem {
                item_type: "binary".to_string(),
                path: loc.clone(),
            });
            println!("Found: Binary at {}", loc.display());
        }
    }

    // Check for data directories
    for data_dir in &data_dirs {
        if data_dir.is_dir() {
            let mut session_count = 0;
            let mut profile_count = 0;
            let profiles_dir = data_dir.join("profiles");
            if profiles_dir.is_dir() {
                if let Ok(entries) = fs::read_dir(&profiles_dir) {
                    for entry in entries.flatten() {
                        if entry.path().is_dir() {
                            profile_count += 1;
                            let sessions_file = entry.path().join("sessions.json");
                            if let Ok(content) = fs::read_to_string(&sessions_file) {
                                session_count += content.matches("\"id\"").count();
                            }
                        }
                    }
                }
            }

            found_items.push(FoundItem {
                item_type: "data".to_string(),
                path: data_dir.clone(),
            });
            println!("Found: Data directory at {}", data_dir.display());
            println!(
                "       {} profiles, {} sessions",
                profile_count, session_count
            );
        }
    }

    // Check for tmux config
    let tmux_conf = home_dir.join(".tmux.conf");
    if let Ok(content) = fs::read_to_string(&tmux_conf) {
        if content.contains("# agent-of-empires configuration") {
            found_items.push(FoundItem {
                item_type: "tmux".to_string(),
                path: tmux_conf.clone(),
            });
            println!("Found: tmux configuration in ~/.tmux.conf");
        }
    }

    println!();

    if found_items.is_empty() {
        println!("Agent of Empires does not appear to be installed.");
        return Ok(());
    }

    // Summary
    println!("The following will be removed:");
    println!();

    for item in &found_items {
        match item.item_type.as_str() {
            "homebrew" => println!("  • Homebrew package: aoe"),
            "binary" => println!("  • Binary: {}", item.path.display()),
            "data" => {
                if args.keep_data {
                    println!("  ○ Data directory: {} (keeping)", item.path.display());
                } else {
                    println!("  • Data directory: {}", item.path.display());
                    println!("    Including: sessions, logs, config");
                }
            }
            "tmux" => {
                if args.keep_tmux_config {
                    println!("  ○ tmux config: ~/.tmux.conf (keeping)");
                } else {
                    println!("  • tmux config block in ~/.tmux.conf");
                }
            }
            _ => {}
        }
    }

    println!();

    // Confirm
    if !args.yes && !args.dry_run {
        print!("Proceed with uninstall? [y/N] ");
        io::stdout().flush()?;

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;

        if response.trim().to_lowercase() != "y" {
            println!("Uninstall cancelled.");
            return Ok(());
        }
        println!();
    }

    if args.dry_run {
        println!("Dry run complete. No changes made.");
        return Ok(());
    }

    println!("Uninstalling...");
    println!();

    // Remove AoE hooks from agent settings files (e.g. ~/.claude/settings.json)
    crate::hooks::uninstall_all_hooks();

    // Perform uninstall
    for item in &found_items {
        match item.item_type.as_str() {
            "homebrew" => {
                println!("Removing Homebrew package...");
                let _ = Command::new("brew").args(["uninstall", "aoe"]).status();
                println!("✓ Homebrew package removed");
            }
            "binary" => {
                println!("Removing binary at {}...", item.path.display());
                if fs::remove_file(&item.path).is_ok() {
                    println!("✓ Binary removed: {}", item.path.display());
                } else {
                    // Try with sudo
                    let _ = Command::new("sudo")
                        .args(["rm", "-f", &item.path.to_string_lossy()])
                        .status();
                }
            }
            "data" if !args.keep_data => {
                println!("Removing data directory...");
                if fs::remove_dir_all(&item.path).is_ok() {
                    println!("✓ Data directory removed: {}", item.path.display());
                }
            }
            "tmux" if !args.keep_tmux_config => {
                println!("Removing tmux configuration...");
                if let Ok(content) = fs::read_to_string(&item.path) {
                    // Backup
                    let backup_path = format!("{}.bak.aoe-uninstall", item.path.display());
                    let _ = fs::write(&backup_path, &content);

                    // Remove agent-of-empires config block
                    let start_marker = "# agent-of-empires configuration";
                    let end_marker = "# End agent-of-empires configuration";

                    if let (Some(start), Some(end)) =
                        (content.find(start_marker), content.find(end_marker))
                    {
                        let end = end + end_marker.len();
                        let mut new_content = format!("{}{}", &content[..start], &content[end..]);
                        while new_content.contains("\n\n\n") {
                            new_content = new_content.replace("\n\n\n", "\n\n");
                        }
                        new_content = new_content.trim_end().to_string() + "\n";
                        if fs::write(&item.path, new_content).is_ok() {
                            println!("✓ tmux configuration removed (backup: {})", backup_path);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    println!();
    println!("╔════════════════════════════════════════╗");
    println!("║     Uninstall complete!                ║");
    println!("╚════════════════════════════════════════╝");
    println!();

    if args.keep_data {
        let preserved: Vec<_> = found_items
            .iter()
            .filter(|i| i.item_type == "data")
            .collect();
        for item in preserved {
            println!("Note: Data directory preserved at {}", item.path.display());
        }
    }

    if args.keep_tmux_config {
        println!("Note: tmux config preserved in ~/.tmux.conf");
    }

    println!();
    println!("Thank you for using Agent of Empires!");
    println!("Feedback: https://github.com/agent-of-empires/agent-of-empires/issues");

    Ok(())
}
