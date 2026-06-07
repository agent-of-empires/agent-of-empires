//! `hmp logs` - view the configured HMP log file with a pretty viewer.
//!
//! Resolves the path from `[logging].file_path`, picks the best available
//! viewer (lnav > bat > less > plain stdout), and prints a one-line tip when
//! `lnav` is missing so users know there's a better experience available.

use anyhow::Result;
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Args)]
pub struct LogsArgs {
    /// Live-tail the log.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Show only the last N lines (fallback viewers; lnav handles its own).
    #[arg(short = 'n', long, value_name = "N")]
    pub lines: Option<usize>,

    /// Skip viewer detection; write plain log to stdout.
    #[arg(long)]
    pub no_pager: bool,

    /// Print the resolved log file path and exit (no viewing).
    #[arg(long)]
    pub path: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Viewer {
    Lnav,
    Bat,
    Less,
    PlainStdout,
}

fn resolved_log_path() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?;
    let cfg = crate::session::load_config()
        .ok()
        .flatten()
        .map(|c| c.logging)
        .unwrap_or_default();
    Ok(crate::logging::resolve_log_path(&cfg, &dir))
}

pub fn detect_viewer(no_pager: bool) -> Viewer {
    if no_pager {
        return Viewer::PlainStdout;
    }
    if which::which("lnav").is_ok() {
        return Viewer::Lnav;
    }
    if which::which("bat").is_ok() {
        return Viewer::Bat;
    }
    if which::which("less").is_ok() {
        return Viewer::Less;
    }
    Viewer::PlainStdout
}

fn viewer_name(v: Viewer) -> &'static str {
    match v {
        Viewer::Lnav => "lnav",
        Viewer::Bat => "bat",
        Viewer::Less => "less",
        Viewer::PlainStdout => "plain stdout",
    }
}

#[tracing::instrument(target = "cli.logs", skip_all)]
pub async fn run(args: LogsArgs) -> Result<()> {
    let target_path = resolved_log_path()?;

    if args.path {
        println!("{}", target_path.display());
        return Ok(());
    }

    if !target_path.exists() {
        eprintln!("{} does not exist (yet).", target_path.display());
        eprintln!("Tip: start `hmp` (TUI) or `hmp serve`, or run with HMP_LOG_LEVEL=debug.");
        return Ok(());
    }

    let viewer = detect_viewer(args.no_pager);
    if !args.no_pager && viewer != Viewer::Lnav && std::env::var_os("HMP_NO_LNAV_TIP").is_none() {
        eprintln!(
            "Tip: install `lnav` for color, level filters, and search (https://lnav.org). \
             Set HMP_NO_LNAV_TIP=1 to silence. Falling back to {}.",
            viewer_name(viewer)
        );
    }

    run_viewer(viewer, &target_path, &args)
}

fn run_viewer(viewer: Viewer, path: &Path, args: &LogsArgs) -> Result<()> {
    match viewer {
        Viewer::Lnav => {
            // lnav handles --follow natively and ignores --lines.
            Command::new("lnav").arg(path).status()?;
            Ok(())
        }
        Viewer::Bat => {
            // bat has no follow mode; downgrade to less +F or plain tail.
            if args.follow {
                let fallback = if which::which("less").is_ok() {
                    Viewer::Less
                } else {
                    Viewer::PlainStdout
                };
                return run_viewer(fallback, path, args);
            }
            let content = read_content(path, args.lines)?;
            pipe_through(
                Command::new("bat").args(["--paging=always", "-l", "log"]),
                &content,
            )
        }
        Viewer::Less => {
            if args.follow {
                // `less +F` on a file can't seek to "last N"; route through
                // tail when --lines is set so the user only sees the recent
                // window plus live appends.
                if let Some(n) = args.lines {
                    return tail_pipe_into(path, n, Command::new("less").args(["-R", "+F"]));
                }
                Command::new("less")
                    .arg("-R")
                    .arg("+F")
                    .arg(path)
                    .status()?;
                return Ok(());
            }
            let content = read_content(path, args.lines)?;
            pipe_through(Command::new("less").arg("-R"), &content)
        }
        Viewer::PlainStdout => {
            if args.follow {
                let mut cmd = Command::new("tail");
                if let Some(n) = args.lines {
                    cmd.args(["-n", &n.to_string()]);
                }
                cmd.arg("-F").arg(path).status()?;
                return Ok(());
            }
            let content = read_content(path, args.lines)?;
            print!("{}", content);
            Ok(())
        }
    }
}

fn read_content(path: &Path, lines: Option<usize>) -> Result<String> {
    let raw = std::fs::read_to_string(path)?;
    Ok(match lines {
        Some(n) => last_n_lines(&raw, n),
        None => raw,
    })
}

pub fn last_n_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn pipe_through(cmd: &mut Command, content: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::piped()).spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(content.as_bytes());
    }
    let _ = child.wait()?;
    Ok(())
}

/// Spawn `tail -n N -F path` and feed its stdout into `viewer`'s stdin so the
/// viewer keeps following live appends while only showing the last N lines.
/// Kills tail when the viewer exits so we don't leak a background tail.
fn tail_pipe_into(path: &Path, lines: usize, viewer: &mut Command) -> Result<()> {
    use std::process::Stdio;
    let mut tail = Command::new("tail")
        .args(["-n", &lines.to_string(), "-F"])
        .arg(path)
        .stdout(Stdio::piped())
        .spawn()?;
    let tail_out = tail.stdout.take().expect("piped stdout");
    let status = viewer.stdin(Stdio::from(tail_out)).status();
    let _ = tail.kill();
    let _ = tail.wait();
    status?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_viewer_no_pager_returns_plain_stdout() {
        assert_eq!(detect_viewer(true), Viewer::PlainStdout);
    }

    #[test]
    fn last_n_lines_returns_tail_and_preserves_trailing_newline() {
        let input = "a\nb\nc\nd\ne\n";
        assert_eq!(last_n_lines(input, 2), "d\ne\n");
    }

    #[test]
    fn last_n_lines_no_trailing_newline_in_input() {
        let input = "a\nb\nc";
        assert_eq!(last_n_lines(input, 2), "b\nc");
    }

    #[test]
    fn last_n_lines_zero_returns_empty() {
        assert_eq!(last_n_lines("a\nb\n", 0), "");
    }

    #[test]
    fn last_n_lines_n_larger_than_input_returns_full_input() {
        let input = "a\nb\n";
        assert_eq!(last_n_lines(input, 100), "a\nb\n");
    }

    #[test]
    fn last_n_lines_empty_input() {
        assert_eq!(last_n_lines("", 5), "");
    }
}
