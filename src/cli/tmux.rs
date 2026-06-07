//! `aoe tmux` command implementation

use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub enum TmuxCommands {
    /// Output session info for use in custom tmux status bar
    ///
    /// Add this to your ~/.tmux.conf:
    ///   set -g status-right "#(aoe tmux status)"
    Status(TmuxStatusArgs),
}

#[derive(Args)]
pub struct TmuxStatusArgs {
    /// Output format (text or json)
    #[arg(short, long, default_value = "text")]
    format: String,
}

#[tracing::instrument(target = "tmux.status", skip_all)]
pub fn run_status(args: TmuxStatusArgs) -> Result<()> {
    use crate::tmux::status_bar::get_session_info_for_current;

    match get_session_info_for_current() {
        Some(info) => {
            if args.format == "json" {
                let json = serde_json::json!({
                    "title": info.title,
                    "branch": info.branch,
                    "sandbox": info.sandbox,
                });
                println!("{}", serde_json::to_string(&json)?);
            } else {
                let mut output = format!("hmp: {}", info.title);
                if let Some(b) = &info.branch {
                    output.push_str(" | ");
                    output.push_str(b);
                }
                if let Some(s) = &info.sandbox {
                    output.push_str(" [");
                    output.push_str(s);
                    output.push(']');
                }
                print!("{}", output);
            }
        }
        None => {
            // Not in an hmp session - output nothing (cleaner for tmux status bar)
            if args.format == "json" {
                println!("null");
            }
        }
    }

    Ok(())
}
