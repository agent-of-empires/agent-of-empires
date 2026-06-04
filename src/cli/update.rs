//! `aoe update` command - self-update by detected install method.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::{self, IsTerminal, Write};
#[cfg(feature = "serve")]
use std::path::Path;

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
    if let InstallMethod::Tarball { binary_path } = &method {
        eprintln!();
        println!(
            "✓ Updated to v{}. Restart `aoe` to use the new version.",
            info.latest_version
        );
        #[cfg(feature = "serve")]
        handle_daemon_restart_after_update(binary_path, args.yes)?;
        #[cfg(not(feature = "serve"))]
        {
            let _ = binary_path;
            println!("{}", daemon_restart_hint());
        }
        println!("{}", completion_refresh_hint());
    } else if matches!(&method, InstallMethod::Homebrew) {
        println!("✓ brew upgrade complete.");
        // Homebrew swaps a Cellar/bin binary whose path we cannot reliably
        // resolve from this process, so we never auto-restart; point the
        // user at the manual step when a daemon is up.
        #[cfg(feature = "serve")]
        if crate::cli::serve::daemon_pid().is_some() {
            println!("{}", daemon_restart_hint());
        }
    }
    Ok(())
}

/// What to do with a running daemon after a successful in-place update.
#[cfg(feature = "serve")]
#[derive(Debug, PartialEq, Eq)]
enum RestartDecision {
    /// Nothing to restart automatically: no self-managed daemon is up, or
    /// the context (non-interactive, no `-y`) cannot drive a restart.
    NotApplicable,
    /// Interactive terminal: ask before restarting.
    Prompt,
    /// Restart without asking (`-y`).
    Auto,
}

/// Decide whether to restart the daemon after an update. A daemon is only
/// restartable when it is both running AND left a `serve.launch` behind
/// (i.e. it was started by `aoe serve --daemon`, not run in the
/// foreground or under a service supervisor). Pure so the matrix is unit
/// testable.
#[cfg(feature = "serve")]
fn restart_decision(
    running: bool,
    launch_present: bool,
    is_tty: bool,
    yes: bool,
) -> RestartDecision {
    if !running || !launch_present {
        return RestartDecision::NotApplicable;
    }
    if yes {
        RestartDecision::Auto
    } else if is_tty {
        RestartDecision::Prompt
    } else {
        // Non-TTY without -y: cannot prompt, so fall back to the hint.
        RestartDecision::NotApplicable
    }
}

/// After an in-place tarball update, offer to restart a self-managed
/// `aoe serve` daemon so it runs the new binary. The restart re-execs the
/// freshly installed binary as `aoe serve --restart` so the new code, not
/// this old in-memory image (whose `current_exe()` may now point at the
/// replaced/unlinked inode), spawns the replacement daemon.
#[cfg(feature = "serve")]
fn handle_daemon_restart_after_update(binary_path: &Path, yes: bool) -> Result<()> {
    use crate::cli::serve;
    let running = serve::daemon_pid().is_some();
    let launch_present = serve::serve_launch_exists();
    match restart_decision(running, launch_present, io::stdin().is_terminal(), yes) {
        RestartDecision::NotApplicable => {
            // Nothing running means no hint is needed. Otherwise point at
            // the right manual step: a self-managed daemon we just cannot
            // drive right now (non-interactive, no -y) restarts with
            // `aoe serve --restart`, but a foreground or supervised daemon
            // (no launch state) must be bounced by its own manager, for
            // which --restart would correctly refuse.
            if running && launch_present {
                println!("{}", daemon_restart_hint());
            } else if running {
                println!("{}", external_restart_hint());
            }
        }
        RestartDecision::Prompt => {
            print!("Restart the running aoe serve daemon now? [Y/n] ");
            io::stdout().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            let answer = answer.trim().to_lowercase();
            if answer.is_empty() || answer == "y" || answer == "yes" {
                restart_via_new_binary(binary_path);
            } else {
                println!("{}", daemon_restart_hint());
            }
        }
        RestartDecision::Auto => restart_via_new_binary(binary_path),
    }
    Ok(())
}

/// Hint for a running daemon that aoe did not start itself (foreground,
/// or under systemd/launchd): `aoe serve --restart` would refuse it, so
/// point the user at the supervisor that owns the process instead.
#[cfg(feature = "serve")]
fn external_restart_hint() -> &'static str {
    "  An `aoe serve` daemon is running but was not started by\n  \
     `aoe serve --daemon`; restart it through whatever launched it (your\n  \
     service manager, or the terminal it runs in) so it picks up the new\n  \
     binary."
}

/// Spawn the freshly installed binary as `aoe serve --restart`. Best
/// effort: on any failure we fall back to the manual hint rather than
/// failing the whole update, which already succeeded.
#[cfg(feature = "serve")]
fn restart_via_new_binary(binary_path: &Path) {
    println!("Restarting daemon…");
    match std::process::Command::new(binary_path)
        .args(["serve", "--restart"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("Daemon restart exited with status {status}.");
            println!("{}", daemon_restart_hint());
        }
        Err(e) => {
            eprintln!("Failed to launch `aoe serve --restart`: {e}");
            println!("{}", daemon_restart_hint());
        }
    }
}

/// Reminder printed after a successful in-place update: a running
/// `aoe serve` daemon keeps executing the old code it already loaded
/// until it is restarted, and its structured view workers survive that restart by
/// design (see #1037). The new binary therefore does not take effect
/// anywhere until the daemon restarts; once it does, a worker left on the
/// old build finishes any in-flight turn and then respawns on the new
/// build automatically (see #1754). Surfacing this avoids the silent
/// mixed-version trap where a freshly-shipped fix appears not to work.
fn daemon_restart_hint() -> &'static str {
    "  If `aoe serve` is running, restart it (`aoe serve --restart`) so the daemon\n  \
     picks up the new binary. Acp workers from the old build finish their\n  \
     current turn, then respawn on the new build."
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
    use super::{completion_refresh_hint, daemon_restart_hint};

    #[test]
    fn hint_points_at_regen_and_eval_alternative() {
        let hint = completion_refresh_hint();
        assert!(hint.contains("aoe completion"));
        assert!(hint.contains("guides/shell-completions"));
        // Mentions the always-fresh alternative so users can avoid manual refresh.
        assert!(hint.to_lowercase().contains("eval"));
    }

    #[test]
    fn daemon_hint_mentions_restart_and_respawn() {
        let hint = daemon_restart_hint();
        // Points the user at the restart that actually applies the binary.
        assert!(hint.contains("aoe serve --restart"));
        // Sets the expectation that workers converge to the new build.
        assert!(hint.to_lowercase().contains("respawn"));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn restart_decision_matrix() {
        use super::{restart_decision, RestartDecision};
        // Nothing running: never restart.
        assert_eq!(
            restart_decision(false, false, true, true),
            RestartDecision::NotApplicable
        );
        // Running but no launch state (foreground / supervised): hands off.
        assert_eq!(
            restart_decision(true, false, true, true),
            RestartDecision::NotApplicable
        );
        // Self-managed daemon + -y: restart without asking.
        assert_eq!(
            restart_decision(true, true, false, true),
            RestartDecision::Auto
        );
        // Self-managed daemon on a TTY without -y: prompt.
        assert_eq!(
            restart_decision(true, true, true, false),
            RestartDecision::Prompt
        );
        // Self-managed daemon, non-TTY, no -y: cannot prompt, fall back.
        assert_eq!(
            restart_decision(true, true, false, false),
            RestartDecision::NotApplicable
        );
    }
}
