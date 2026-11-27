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
pub async fn run(args: SuggestArgs, profile: Option<&str>) -> Result<()> {
    if args.title.trim().is_empty() || args.prompt.trim().is_empty() {
        bail!("Title and --prompt cannot be empty");
    }
    let client = HttpClient::new(require_daemon().await?)?;
    let title = args.title.clone();
    let id = client.push_suggestion(&push_body(args, profile)).await?;
    println!("Suggested '{title}' ({id})");
    Ok(())
}

/// The push request; `expected_profile` makes a daemon serving another profile refuse it.
fn push_body(args: SuggestArgs, profile: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "expected_profile": profile,
        "id": args.id,
        "title": args.title,
        "reason": args.reason,
        "prompt": args.prompt,
        "project": args.project,
        "pushed_by": "CLI",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_push_names_the_profile_only_when_one_was_chosen() {
        let args = || SuggestArgs {
            title: "T".into(),
            prompt: "P".into(),
            reason: String::new(),
            project: None,
            id: None,
        };
        assert_eq!(push_body(args(), Some("work"))["expected_profile"], "work");
        assert!(push_body(args(), None)["expected_profile"].is_null());
    }
}
