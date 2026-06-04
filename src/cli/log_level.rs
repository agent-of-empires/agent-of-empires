//! `aoe log-level` — get or change the running daemon's tracing filter.
//!
//! Calls the daemon's `/api/log-level` endpoint over HTTP using the
//! token embedded in `serve.url`. Does not require the daemon to be
//! daemonised — a foreground `aoe serve` is reachable if its
//! `serve.url` file is current.

use anyhow::{bail, Context, Result};
use clap::Args;

use super::serve::read_serve_urls;

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
    let urls = read_serve_urls();
    let Some(primary) = urls.first() else {
        bail!("No aoe serve daemon is running, or serve.url is empty/missing.");
    };

    let base = primary
        .url
        .split('?')
        .next()
        .unwrap_or(&primary.url)
        .trim_end_matches('/');
    let endpoint = format!("{base}/api/log-level");
    let token = extract_token(&primary.url);

    let client = reqwest::Client::new();

    if args.get || (args.level.is_none() && args.filter.is_none()) {
        let mut req = client.get(&endpoint);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.context("GET /api/log-level")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("server returned {status}: {body}");
        }
        println!("{body}");
        return Ok(());
    }

    let body = match (args.level.as_deref(), args.filter.as_deref()) {
        (Some(level), None) => {
            if matches_known_level(level) {
                serde_json::json!({ "level": level })
            } else {
                bail!(
                    "{level:?} is not a known level. \
                     Use trace|debug|info|warn|error, or pass --filter for raw EnvFilter syntax."
                );
            }
        }
        (None, Some(filter)) => serde_json::json!({ "filter": filter }),
        (Some(_), Some(_)) => bail!("specify exactly one of <level> or --filter"),
        (None, None) => unreachable!("guarded above"),
    };

    let mut req = client.patch(&endpoint).json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.context("PATCH /api/log-level")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("server returned {status}: {text}");
    }
    println!("{text}");
    Ok(())
}

fn matches_known_level(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "trace" | "debug" | "info" | "warn" | "warning" | "error"
    )
}

fn extract_token(url: &str) -> Option<&str> {
    let query = url.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        if let Some(rest) = pair.strip_prefix("token=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest);
        }
    }
    None
}
