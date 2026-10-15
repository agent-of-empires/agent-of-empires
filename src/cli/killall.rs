//! `aoe killall`: a panic button that stops then force-kills everything aoe is

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct KillallArgs {
    /// Grace period in seconds before force-killing agent workers. tmux
    /// sessions and the daemon use their own built-in grace.
    #[arg(long, default_value_t = 5)]
    pub timeout_secs: u64,

    /// Leave the `aoe serve` daemon running; stop only workers and tmux
    /// sessions.
    #[arg(long)]
    pub keep_daemon: bool,
}

pub async fn run(args: KillallArgs) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();

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

    match crate::cli::acp::stop_all_workers(args.timeout_secs).await {
        Ok(n) => println!("Stopped {n} agent worker(s)."),
        Err(e) => errors.push(format!("workers: {e}")),
    }

    match crate::tmux::stop_all_sessions() {
        Ok(n) => println!("Stopped {n} tmux session(s)."),
        Err(e) => errors.push(format!("tmux: {e}")),
    }

    if !errors.is_empty() {
        for e in &errors {
            eprintln!("killall error: {e}");
        }
        anyhow::bail!("killall completed with {} error(s)", errors.len());
    }

    Ok(())
}

pub fn stop_trap() -> Result<()> {
    anyhow::bail!(
        "`aoe stop` is not a command. Did you mean:\n  \
         aoe session stop <id>   stop one session\n  \
         aoe acp stop [--all]    stop agent workers\n  \
         aoe serve --stop        stop the web daemon\n  \
         aoe killall             force-stop everything"
    )
}
