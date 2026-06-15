//! `aoe stop-all`: a panic button that stops then force-kills everything aoe
//! is running, in one command. Tears down the serve daemon, every ACP cockpit
//! worker, and every aoe tmux session (agent, terminal, container terminal,
//! tool). Each surface is attempted independently; one failing surface never
//! aborts the others, and the exit code is non-zero only if something failed.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct StopAllArgs {
    /// Grace period in seconds before force-killing agent workers. tmux
    /// sessions and the daemon use their own built-in grace.
    #[cfg(feature = "serve")]
    #[arg(long, default_value_t = 5)]
    pub timeout_secs: u64,

    /// Leave the `aoe serve` daemon running; stop only workers and tmux
    /// sessions.
    #[cfg(feature = "serve")]
    #[arg(long)]
    pub keep_daemon: bool,
}

pub async fn run(args: StopAllArgs) -> Result<()> {
    // The daemon and workers are the only fallible, serve-gated surfaces, so
    // the error aggregation lives entirely under `serve`. In a TUI-only build
    // `run` just sweeps tmux and always succeeds.
    #[cfg(not(feature = "serve"))]
    let _ = args;

    #[cfg(feature = "serve")]
    let mut errors: Vec<String> = Vec::new();

    // Daemon first. Removing the orchestrator means the worker sweep below
    // cannot race a daemon-driven respawn; any orphaned workers still die via
    // their recorded process group in that sweep.
    #[cfg(feature = "serve")]
    if !args.keep_daemon {
        if crate::cli::serve::daemon_pid().is_some() {
            match crate::cli::serve::stop_daemon().await {
                Ok(()) => println!("Stopped aoe serve daemon."),
                Err(e) => errors.push(format!("daemon: {e}")),
            }
        } else {
            println!("No aoe serve daemon running.");
        }
    }

    #[cfg(feature = "serve")]
    {
        let n = crate::cli::acp::stop_all_workers(args.timeout_secs).await;
        println!("Stopped {n} agent worker(s).");
    }

    let sessions = crate::tmux::stop_all_sessions();
    println!("Stopped {sessions} tmux session(s).");

    #[cfg(feature = "serve")]
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("stop-all error: {e}");
        }
        anyhow::bail!("stop-all completed with {} error(s)", errors.len());
    }

    Ok(())
}
