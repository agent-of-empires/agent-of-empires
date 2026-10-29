//! `aoe migrate`: run pending data migrations now, with progress on stderr.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::cli::serve::{daemon_status, try_lock_serve, DaemonStatus};
use crate::migrations::progress::{fit_width, ConsoleProgress, Event, Reporter};

/// Run startup migrations. When a managed daemon blocks them and a person is
/// at the terminal, offer `aoe serve --restart`, which also migrates.
pub async fn run_offering_restart(reporter: Option<Reporter>, interactive: bool) -> Result<()> {
    match crate::migrations::run_migrations_with(reporter.clone()) {
        Err(error) => {
            offer_restart(error, interactive).await?;
            crate::migrations::run_migrations_with(reporter)
        }
        ok => ok,
    }
}

/// `Ok` once the daemon was restarted and the migration is worth retrying;
/// otherwise the original `error`.
pub async fn offer_restart(error: anyhow::Error, interactive: bool) -> Result<()> {
    if !error.is::<crate::migrations::DaemonBlocksMigration>() {
        return Err(error);
    }
    let managed =
        crate::cli::serve::daemon_pid().is_some_and(crate::cli::serve::serve_launch_matches);
    let autostart_disabled = crate::acp::client::daemon_manager::autostart_disabled();
    let remote = crate::acp::client::discovery::discover_env().is_some();
    if !should_offer_restart(interactive, autostart_disabled, remote, managed)
        || !confirm_restart()?
    {
        return Err(error);
    }
    eprintln!("Restarting the daemon...");
    crate::cli::serve::restart_daemon_with_receipt(None, false).await?;
    Ok(())
}

fn should_offer_restart(
    interactive: bool,
    autostart_disabled: bool,
    remote: bool,
    managed: bool,
) -> bool {
    interactive && !autostart_disabled && !remote && managed
}

fn confirm_restart() -> Result<bool> {
    eprint!("Restart the daemon now? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

const QUIET_FOR: Duration = Duration::from_millis(1500);

const FALLBACK_WIDTH: usize = 80;

const PREFIX: &str = "aoe: ";

struct StderrRenderer {
    console: ConsoleProgress,
    started: Option<Instant>,
    shown: bool,
    tty: bool,
}

impl StderrRenderer {
    fn new(tty: bool) -> Self {
        Self {
            console: ConsoleProgress::default(),
            started: None,
            shown: false,
            tty,
        }
    }

    fn render(&mut self, event: Event, now: Instant, width: usize) -> String {
        match &event {
            Event::Started { .. } => {
                self.started = Some(now);
                self.shown = false;
            }
            Event::Finished { .. } => self.started = None,
            _ => {}
        }
        let discrete = !matches!(event, Event::Progress(_));
        let notice = matches!(event, Event::Notice(_));
        self.console.apply(event);
        let slow = self
            .started
            .is_some_and(|at| now.duration_since(at) >= QUIET_FOR);
        let newly_shown = !self.shown && (notice || slow);
        if notice || slow {
            self.shown = true;
        }
        let lines = self.console.take_lines();
        if !self.shown && lines.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        if self.tty {
            out.push_str("\r\x1b[2K");
        }
        for line in lines {
            out.push_str(PREFIX);
            out.push_str(&line);
            out.push('\n');
        }
        if let Some(status) = self.console.status_line() {
            if self.tty {
                out.push_str(&fit_width(
                    &format!("{PREFIX}{status}"),
                    width.max(FALLBACK_WIDTH.min(width)),
                ));
            } else if !notice && (discrete || newly_shown) {
                out.push_str(PREFIX);
                out.push_str(&status);
                out.push('\n');
            }
        }
        out
    }
}

pub fn stderr_reporter() -> Reporter {
    let tty = std::io::stderr().is_terminal();
    let renderer = Arc::new(Mutex::new(StderrRenderer::new(tty)));
    Arc::new(move |event: Event| {
        let width = crate::terminal::get_size().map_or(FALLBACK_WIDTH, |(w, _)| w as usize);
        let out =
            renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(event, Instant::now(), width);
        if out.is_empty() {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    })
}

#[derive(Debug, Default, clap::Args)]
#[command(group(clap::ArgGroup::new("recovery_source").args(["restore_purge_journal", "restore_tool_journal"]).multiple(false)))]
pub struct MigrateArgs {
    /// Restore a complete original purge journal from a consistent application backup.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "restore_tool_journal",
        requires = "confirm_complete_original_authority"
    )]
    pub restore_purge_journal: Option<std::path::PathBuf>,
    /// Restore complete original tool retirement receipts from a consistent application backup.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "restore_purge_journal",
        requires = "confirm_complete_original_authority"
    )]
    pub restore_tool_journal: Option<std::path::PathBuf>,
    /// Attest that FILE contains all original authority, not a stale or partial copy.
    #[arg(long, requires = "recovery_source")]
    pub confirm_complete_original_authority: bool,
}

fn refuse_while_daemon_runs(status: DaemonStatus) -> Result<()> {
    match status {
        DaemonStatus::Absent => Ok(()),
        DaemonStatus::Verified(pid) => bail!("aoe serve daemon is running (PID {pid}); stop it with `aoe serve --stop` before running aoe migrate"),
        DaemonStatus::Unverified => bail!("Cannot verify the existing daemon; migration and recovery refused"),
    }
}

pub async fn run(args: &MigrateArgs) -> Result<()> {
    refuse_while_daemon_runs(daemon_status())?;
    let transaction = crate::daemon::lifecycle::Transaction::acquire().await?;
    let Some(serve_lock) = try_lock_serve()? else {
        bail!("an aoe serve daemon is starting; retry `aoe migrate` once it is stopped");
    };
    crate::migrations::ensure_schema_supported()?;
    if let Some(source) = &args.restore_purge_journal {
        anyhow::ensure!(
            args.confirm_complete_original_authority,
            "Explicit complete original authority confirmation is required"
        );
        crate::cli::serve::with_inactive_serve_state(&transaction, Some(&serve_lock), |serve| {
            crate::session::purge_owners::restore_from_complete_backup(source, &transaction, serve)
        })?;
        eprintln!(
            "{PREFIX}restored complete original purge authority; no cleanup effects were run"
        );
        return Ok(());
    }
    if let Some(source) = &args.restore_tool_journal {
        anyhow::ensure!(
            args.confirm_complete_original_authority,
            "Explicit complete original authority confirmation is required"
        );
        crate::cli::serve::with_inactive_serve_state(&transaction, Some(&serve_lock), |serve| {
            crate::tmux::tool_retirement::restore_from_complete_backup(source, &transaction, serve)
        })?;
        eprintln!("{PREFIX}restored complete original tool retirement authority; no process effects were run");
        return Ok(());
    }
    if !crate::migrations::has_pending_migrations() {
        eprintln!("{PREFIX}data schema is current; retrying any deferred migration work");
    }
    crate::migrations::run_migrations_under_exclusion(
        Some(stderr_reporter()),
        true,
        &transaction,
        &serve_lock,
    )?;
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
    match crate::migrations::v027_isolate_sandbox_stores::sessions_on_shared_store()? {
        0 => eprintln!("{PREFIX}migrations complete"),
        left => eprintln!("{PREFIX}migrations complete, except {left} sandboxed session(s) still on a shared agent store"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn offer_restart_returns_the_original_error_unless_offered() {
        use crate::migrations::DaemonBlocksMigration;
        let blocked: anyhow::Error = DaemonBlocksMigration("blocked").into();
        let message = blocked.to_string();
        let error = super::offer_restart(blocked, false).await.unwrap_err();
        assert!(error.is::<DaemonBlocksMigration>());
        assert_eq!(error.to_string(), message);

        let other = super::offer_restart(anyhow::anyhow!("boom"), true)
            .await
            .unwrap_err();
        assert_eq!(other.to_string(), "boom");
    }

    #[test]
    fn restart_is_offered_only_to_a_person_with_a_local_managed_daemon() {
        use super::should_offer_restart;
        for (interactive, autostart_disabled, remote, managed, offered) in [
            (true, false, false, true, true),
            (false, false, false, true, false),
            (true, true, false, true, false),
            (true, false, true, true, false),
            (true, false, false, false, false),
        ] {
            assert_eq!(
                should_offer_restart(interactive, autostart_disabled, remote, managed),
                offered,
                "{interactive} {autostart_disabled} {remote} {managed}"
            );
        }
    }

    use super::*;

    fn started() -> Event {
        Event::Started {
            version: 27,
            name: "isolate_sandbox_stores",
            position: 1,
            total: 1,
        }
    }

    #[test]
    fn live_and_ambiguous_daemon_identities_block_migrate() {
        assert!(refuse_while_daemon_runs(DaemonStatus::Absent).is_ok());
        assert!(refuse_while_daemon_runs(DaemonStatus::Unverified).is_err());
        let error = refuse_while_daemon_runs(DaemonStatus::Verified(4242))
            .unwrap_err()
            .to_string();
        assert!(error.contains("PID 4242") && error.contains("aoe serve --stop"));
    }

    #[test]
    fn a_quick_migration_stays_silent_on_a_pipe() {
        let mut r = StderrRenderer::new(false);
        let t0 = Instant::now();
        assert_eq!(r.render(started(), t0, 80), "");
        assert_eq!(
            r.render(Event::Step("reading session registries".into()), t0, 80),
            ""
        );
        assert_eq!(
            r.render(
                Event::Finished {
                    version: 27,
                    elapsed: Duration::from_millis(300),
                },
                t0,
                80,
            ),
            ""
        );
    }

    #[test]
    fn a_slow_migration_prints_its_first_status_even_from_a_progress_tick() {
        let mut r = StderrRenderer::new(false);
        let t0 = Instant::now();
        r.render(started(), t0, 80);
        r.render(Event::Step("copying agent store 1/2".into()), t0, 80);
        assert_eq!(
            r.render(
                Event::Progress("100 files, 5 MB".into()),
                t0 + Duration::from_millis(500),
                80
            ),
            ""
        );
        let out = r.render(
            Event::Progress("200 files, 11 MB".into()),
            t0 + QUIET_FOR,
            80,
        );
        assert!(out.starts_with(
            "aoe: Data migration v27 (isolate_sandbox_stores): copying agent store 1/2, 200 files"
        ));
        assert!(out.ends_with('\n'));
        assert_eq!(
            r.render(
                Event::Progress("300 files, 17 MB".into()),
                t0 + QUIET_FOR,
                80
            ),
            ""
        );
        let out = r.render(
            Event::Step("retiring shared agent store".into()),
            t0 + QUIET_FOR,
            80,
        );
        assert_eq!(out.matches('\n').count(), 1);
        assert!(out.contains("retiring shared agent store"));
    }

    #[test]
    fn a_notice_prints_at_once_and_reveals_the_context() {
        let mut r = StderrRenderer::new(false);
        let t0 = Instant::now();
        r.render(started(), t0, 80);
        r.render(Event::Step("reading session registries".into()), t0, 80);
        let out = r.render(
            Event::Notice("AOE_DEFER_SANDBOX_MIGRATION is set".into()),
            t0,
            80,
        );
        assert_eq!(out, "aoe: AOE_DEFER_SANDBOX_MIGRATION is set\n");
        let out = r.render(
            Event::Step("checking which sandbox containers are running".into()),
            t0,
            80,
        );
        assert_eq!(out, "aoe: Data migration v27 (isolate_sandbox_stores): checking which sandbox containers are running (0.0s)\n");
    }

    #[test]
    fn a_tty_redraws_one_clamped_status_line() {
        let mut r = StderrRenderer::new(true);
        let t0 = Instant::now();
        r.render(started(), t0, 40);
        let step = "copying agent store 1/2: /home/u/.gemini/sandbox -> /home/u/.gemini/sandbox-v2/0123456789abcdef";
        let out = r.render(Event::Step(step.into()), t0 + QUIET_FOR, 40);
        assert!(out.starts_with("\r\x1b[2K"));
        let status = out.trim_start_matches("\r\x1b[2K");
        assert_eq!(status.chars().count(), 40, "{status:?}");
        assert!(!status.contains('\n'));
        let out = r.render(
            Event::Finished {
                version: 27,
                elapsed: Duration::from_millis(2500),
            },
            t0 + QUIET_FOR,
            40,
        );
        assert_eq!(
            out,
            "\r\x1b[2Kaoe: Data migration v27 (isolate_sandbox_stores) done in 2.5s\n"
        );
    }
}
