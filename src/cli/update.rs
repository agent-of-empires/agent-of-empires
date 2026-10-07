//! `aoe update` command - self-update by detected install method.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::{self, IsTerminal, Write};
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
        handle_daemon_restart_after_update(binary_path, args.yes)?;
        println!("{}", completion_refresh_hint());
    } else if matches!(&method, InstallMethod::Homebrew) {
        println!("✓ brew upgrade complete.");
        if !matches!(
            crate::cli::serve::daemon_status(),
            crate::cli::serve::DaemonStatus::Absent
        ) {
            println!("{}", manual_update_restart_hint());
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum RestartDecision {
    NotApplicable,
    ManualSelfManaged,
    ManualExternal,
    ManualUnverified,
    Prompt,
    Auto,
}

#[derive(Clone, Copy, Debug)]
enum UpdateDaemonState {
    Absent,
    SelfManaged,
    External,
    Unverified,
}

fn restart_decision(daemon: UpdateDaemonState, is_tty: bool, yes: bool) -> RestartDecision {
    match daemon {
        UpdateDaemonState::Absent => RestartDecision::NotApplicable,
        UpdateDaemonState::External => RestartDecision::ManualExternal,
        UpdateDaemonState::Unverified => RestartDecision::ManualUnverified,
        UpdateDaemonState::SelfManaged if yes => RestartDecision::Auto,
        UpdateDaemonState::SelfManaged if is_tty => RestartDecision::Prompt,
        UpdateDaemonState::SelfManaged => RestartDecision::ManualSelfManaged,
    }
}

fn handle_daemon_restart_after_update(binary_path: &Path, yes: bool) -> Result<()> {
    use crate::cli::serve;
    let daemon = match serve::daemon_status() {
        serve::DaemonStatus::Absent => UpdateDaemonState::Absent,
        serve::DaemonStatus::Unverified => UpdateDaemonState::Unverified,
        serve::DaemonStatus::Verified(pid) if serve::serve_launch_matches(pid) => {
            UpdateDaemonState::SelfManaged
        }
        serve::DaemonStatus::Verified(_) => UpdateDaemonState::External,
    };
    match restart_decision(daemon, io::stdin().is_terminal(), yes) {
        RestartDecision::NotApplicable => {}
        RestartDecision::ManualSelfManaged => println!("{}", daemon_restart_hint()),
        RestartDecision::ManualExternal => println!("{}", external_restart_hint()),
        RestartDecision::ManualUnverified => println!("{}", unverified_restart_hint()),
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

fn external_restart_hint() -> &'static str {
    "  WARNING: an `aoe serve` daemon is running but was not started by\n  \
     `aoe serve --daemon`; restart it through whatever launched it (your\n  \
     service manager, or the terminal it runs in) so it picks up the new\n  \
     binary."
}

fn unverified_restart_hint() -> &'static str {
    "  WARNING: aoe found daemon state but could not verify the running process.\n  \
     Existing `aoe serve` processes keep running the old build until restarted.\n  \
     This usually means the daemon belongs to another user (a root service unit,\n  \
     or `sudo aoe serve`), and `aoe serve --restart` refuses what it cannot\n  \
     verify: restart it as its owner, or through its terminal or service manager."
}

fn manual_update_restart_hint() -> &'static str {
    "  WARNING: existing `aoe serve` processes keep running the old build until\n  \
     restarted. If started with `aoe serve --daemon`, run `aoe serve --restart`;\n  \
     otherwise restart it through its terminal or service manager."
}

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

fn daemon_restart_hint() -> &'static str {
    "  WARNING: if `aoe serve` is running, restart it (`aoe serve --restart`) so the daemon\n  \
     picks up the new binary. Acp workers from the old build finish their\n  \
     current turn, then respawn on the new build."
}

fn completion_refresh_hint() -> &'static str {
    "  If you use static shell completions, regenerate them so they pick up new\n  \
     commands, e.g. `aoe completion zsh > ~/.zfunc/_aoe`. Eval-on-startup setups\n  \
     stay in sync automatically: https://www.agent-of-empires.com/guides/shell-completions/"
}

#[cfg(test)]
mod tests {
    use super::{completion_refresh_hint, daemon_restart_hint};

    #[test]
    fn hints_name_their_recovery_commands() {
        let hint = completion_refresh_hint();
        assert!(hint.contains("aoe completion"));
        assert!(hint.contains("guides/shell-completions"));
        assert!(hint.to_lowercase().contains("eval"));

        let hint = daemon_restart_hint();
        assert!(hint.contains("WARNING:"));
        assert!(hint.contains("aoe serve --restart"));
        assert!(hint.to_lowercase().contains("respawn"));

        let fallback = super::unverified_restart_hint();
        assert!(fallback.contains("belongs to another user"));
        assert!(fallback.contains("terminal or service manager"));

        let external = super::external_restart_hint();
        assert!(external.contains("WARNING:"));
        assert!(!external.contains("aoe serve --restart"));
        assert!(external.contains("service manager"));

        let manual = super::manual_update_restart_hint();
        assert!(manual.contains("WARNING:"));
        assert!(manual.contains("aoe serve --restart"));
        assert!(manual.contains("terminal or service manager"));
    }

    #[test]
    fn restart_decision_matrix() {
        use super::{restart_decision, RestartDecision, UpdateDaemonState};
        let cases = [
            (
                UpdateDaemonState::Absent,
                true,
                true,
                RestartDecision::NotApplicable,
            ),
            (
                UpdateDaemonState::Unverified,
                false,
                true,
                RestartDecision::ManualUnverified,
            ),
            (
                UpdateDaemonState::External,
                true,
                true,
                RestartDecision::ManualExternal,
            ),
            (
                UpdateDaemonState::SelfManaged,
                false,
                true,
                RestartDecision::Auto,
            ),
            (
                UpdateDaemonState::SelfManaged,
                true,
                false,
                RestartDecision::Prompt,
            ),
            (
                UpdateDaemonState::SelfManaged,
                false,
                false,
                RestartDecision::ManualSelfManaged,
            ),
        ];
        for (daemon, is_tty, yes, expected) in cases {
            assert_eq!(restart_decision(daemon, is_tty, yes), expected);
        }
    }
}
