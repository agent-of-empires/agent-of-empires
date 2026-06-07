//! `aoe url` command -- print the live dashboard URL of a running `hmp serve` daemon.

use anyhow::{bail, Result};
use clap::Args;

use super::serve::{daemon_pid, read_serve_urls, ServeUrl};

#[derive(Args)]
pub struct UrlArgs {
    /// Print every labeled URL (Tailscale / LAN / localhost) on its own line.
    /// The primary URL is printed first as `primary\t<url>`; alternates use
    /// `<label>\t<url>`. The tab-separated format makes the output easy to
    /// parse from shell scripts.
    #[arg(long)]
    pub all: bool,

    /// Print only the auth token from the primary URL's `?token=` query
    /// parameter. Useful for scripted login flows or pasting into the PWA.
    /// Exits non-zero when the URL has no token (e.g. `--no-auth` server).
    #[arg(long, conflicts_with = "all")]
    pub token_only: bool,
}

#[tracing::instrument(target = "cli.serve", skip_all)]
pub fn run(args: UrlArgs) -> Result<()> {
    if daemon_pid().is_none() {
        bail!(
            "No hmp serve daemon is running.\n\
             Start one with: hmp serve --daemon"
        );
    }

    let urls = read_serve_urls();
    let Some(primary) = urls.first() else {
        bail!("Daemon is running but the URL file is empty or missing.");
    };

    if args.token_only {
        match extract_token(&primary.url) {
            Some(tok) => {
                println!("{}", tok);
                Ok(())
            }
            None => bail!("Primary URL has no `?token=` query parameter (server may be running with --no-auth)."),
        }
    } else if args.all {
        for u in &urls {
            println!("{}", format_labeled(u));
        }
        Ok(())
    } else {
        println!("{}", primary.url);
        Ok(())
    }
}

fn format_labeled(u: &ServeUrl) -> String {
    let label = u.label.as_deref().unwrap_or("primary");
    format!("{}\t{}", label, u.url)
}

/// Extract the `token` query parameter from a URL. Returns `None` if the
/// URL has no query string or no `token=` key. Avoids pulling in a full
/// URL parser for one parameter; the auth token has no special characters.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_finds_simple_token() {
        assert_eq!(
            extract_token("http://localhost:8080/?token=abc123"),
            Some("abc123")
        );
    }

    #[test]
    fn extract_token_returns_none_when_missing() {
        assert_eq!(extract_token("http://localhost:8080/"), None);
        assert_eq!(extract_token("http://localhost:8080/?foo=bar"), None);
    }

    #[test]
    fn extract_token_returns_none_for_empty_value() {
        assert_eq!(extract_token("http://localhost:8080/?token="), None);
    }

    #[test]
    fn extract_token_handles_multi_param_query() {
        assert_eq!(
            extract_token("http://localhost:8080/?foo=bar&token=zzz"),
            Some("zzz")
        );
    }

    #[test]
    fn format_labeled_uses_primary_for_unlabeled() {
        let u = ServeUrl {
            label: None,
            url: "http://x".into(),
        };
        assert_eq!(format_labeled(&u), "primary\thttp://x");
    }

    #[test]
    fn format_labeled_uses_label_when_present() {
        let u = ServeUrl {
            label: Some("lan".into()),
            url: "http://x".into(),
        };
        assert_eq!(format_labeled(&u), "lan\thttp://x");
    }
}
