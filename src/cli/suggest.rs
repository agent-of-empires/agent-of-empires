//! `aoe suggest`: put a suggested session on the Suggested sessions list.

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::{require_daemon, HttpClient};

#[derive(Args)]
pub struct SuggestArgs {
    /// Short title of the suggested session
    title: String,

    /// The first prompt the suggested session starts with
    #[arg(long)]
    prompt: String,

    /// Why this is worth doing now
    #[arg(long, default_value = "")]
    reason: String,

    /// Registered Project name or path the session belongs in
    #[arg(long)]
    project: Option<String>,

    /// A stable key; suggesting the same one again replaces that suggestion
    #[arg(long)]
    id: Option<String>,
}

#[tracing::instrument(target = "cli.suggest", skip_all)]
pub async fn run(args: SuggestArgs) -> Result<()> {
    if args.title.trim().is_empty() || args.prompt.trim().is_empty() {
        bail!("Title and --prompt cannot be empty");
    }
    let client = HttpClient::new(require_daemon().await?)?;
    let id = client
        .push_suggestion(&serde_json::json!({
            "id": args.id,
            "title": args.title,
            "reason": args.reason,
            "prompt": args.prompt,
            "project": args.project,
            "pushed_by": "CLI",
        }))
        .await?;
    println!("Suggested '{}' ({id})", args.title);
    Ok(())
}
