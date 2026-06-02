//! `aoe update` command - self-update by detected install method.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::{self, IsTerminal, Write};

use crate::update::check_for_update;
use crate::update::install::{
    detect_install_method, format_prompt_block, parent_is_writable, perform_update, InstallMethod,
};

#[derive(Args)]
pub struct UpdateArgs {
    /// Skip confirmation prompt
    #[arg(short = 'y', long)]
    yes: bool,

    /// Print update status and exit (no install)
    #[arg(long)]
    check: bool,

    /// Detect install method and print what would happen, no download
    #[arg(long)]
    dry_run: bool,
}

#[tracing::instrument(target = "cli.session", skip_all)]
pub async fn run(args: UpdateArgs) -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    // Force-fresh check; the user explicitly asked.
    let info = check_for_update(current_version, true)
        .await
        .context("checking for updates")?;

    if args.check {
        println!("current: {}", info.current_version);
        println!("latest:  {}", info.latest_version);
        println!("available: {}", info.available);
        return Ok(());
    }

    if !info.available {
        println!(
            "You're on v{} (latest). Nothing to do.",
            info.current_version
        );
        return Ok(());
    }

    let method = detect_install_method()?;

    // For non-auto-updatable install methods, just print the upgrade
    // instructions and exit. No prompt, since there's nothing to confirm.
    if matches!(
        &method,
        InstallMethod::Nix | InstallMethod::Cargo | InstallMethod::Unknown { .. }
    ) {
        perform_update(&method, &info.latest_version, None).await?;
        return Ok(());
    }

    let needs_sudo = matches!(&method, InstallMethod::Tarball { binary_path } if !parent_is_writable(binary_path));

    let prompt = format_prompt_block(
        &info.current_version,
        &info.latest_version,
        &method,
        needs_sudo,
    );
    println!("{prompt}\n");

    if args.dry_run {
        println!("(dry run; not downloading)");
        return Ok(());
    }

    if !args.yes {
        if !io::stdin().is_terminal() {
            bail!("stdin is not a TTY; pass `-y` to confirm.");
        }
        print!("Proceed? [Y/n] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_lowercase();
        if !(answer.is_empty() || answer == "y" || answer == "yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let mut last_pct: i64 = -1;
    let mut on_progress = |bytes: u64, total: Option<u64>| {
        if let Some(total) = total {
            let pct = (bytes as f64 / total as f64 * 100.0) as i64;
            if pct != last_pct && pct % 5 == 0 {
                eprint!("\rDownloading… {pct}%");
                let _ = io::stderr().flush();
                last_pct = pct;
            }
        }
    };
    perform_update(&method, &info.latest_version, Some(&mut on_progress)).await?;
    if matches!(&method, InstallMethod::Tarball { .. }) {
        eprintln!();
        println!(
            "✓ Updated to v{}. Restart `aoe` to use the new version.",
            info.latest_version
        );
        println!("{}", completion_refresh_hint());
    } else if matches!(&method, InstallMethod::Homebrew) {
        println!("✓ brew upgrade complete.");
    }
    Ok(())
}

/// Reminder printed after a successful in-place update. A static completion
/// file does not refresh itself, so it goes stale once the new binary adds or
/// renames commands. We deliberately do not rewrite the file: aoe does not
/// track which paths the user installed completions to, and overwriting files
/// it does not own (dotfile-managed symlinks, system paths) is unsafe. The
/// eval-on-startup setup avoids the problem entirely.
fn completion_refresh_hint() -> &'static str {
    "  If you use static shell completions, regenerate them so they pick up new\n  \
     commands, e.g. `aoe completion zsh > ~/.zfunc/_aoe`. Eval-on-startup setups\n  \
     stay in sync automatically: https://www.agent-of-empires.com/guides/shell-completions/"
}

#[cfg(test)]
mod tests {
    use super::completion_refresh_hint;

    #[test]
    fn hint_points_at_regen_and_eval_alternative() {
        let hint = completion_refresh_hint();
        assert!(hint.contains("aoe completion"));
        assert!(hint.contains("guides/shell-completions"));
        // Mentions the always-fresh alternative so users can avoid manual refresh.
        assert!(hint.to_lowercase().contains("eval"));
    }
}
