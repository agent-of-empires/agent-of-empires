//! `aoe log-level` — get or change the running daemon's tracing filter.

use anyhow::{bail, Context, Result};
use clap::Args;

#[derive(Args)]
pub struct LogLevelArgs {
    /// Bare level (trace|debug|info|warn|error). Expands to all known
    /// target roots, avoiding the firehose of dependency logs you would
    /// get from `RUST_LOG=debug`.
    pub level: Option<String>,

    /// Raw EnvFilter directive. Use this for per-target tuning, e.g.
    /// `--filter acp.protocol=trace,info`. Bare `--filter debug` is
    /// rejected; use the positional `level` form instead.
    #[arg(long)]
    pub filter: Option<String>,

    /// Print the current filter without changing it.
    #[arg(long, conflicts_with_all = ["level", "filter"])]
    pub get: bool,
}

#[tracing::instrument(target = "cli.log_level", skip_all)]
pub async fn run(args: LogLevelArgs) -> Result<()> {
    let endpoint = crate::acp::client::discovery::discover_local()
        .context("No aoe serve daemon is running.")?;
    let client = crate::acp::client::HttpClient::new(endpoint)?;

    let patch = match (args.level.as_deref(), args.filter.as_deref()) {
        _ if args.get => None,
        (None, None) => None,
        (Some(level), None) if matches_known_level(level) => {
            Some(serde_json::json!({ "level": level }))
        }
        (Some(level), None) => bail!(
            "{level:?} is not a known level. \
             Use trace|debug|info|warn|error, or pass --filter for raw EnvFilter syntax."
        ),
        (None, Some(filter)) => Some(serde_json::json!({ "filter": filter })),
        (Some(_), Some(_)) => bail!("specify exactly one of <level> or --filter"),
    };

    println!(
        "{}",
        serde_json::to_string(&client.log_level(patch.as_ref()).await?)?
    );
    Ok(())
}

fn matches_known_level(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "trace" | "debug" | "info" | "warn" | "warning" | "error"
    )
}
