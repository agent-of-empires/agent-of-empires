//! `aoe remote`: manage the daemon endpoints the TUI can connect to.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};

use reqwest::StatusCode;

use crate::daemon::login::{self, LoginError};
use crate::daemon::remotes::{self, Remote};
use crate::daemon::{DaemonClient, DaemonClientError};

#[derive(Subcommand)]
pub enum RemoteCommands {
    /// Add or update a remote daemon endpoint
    Add(RemoteAddArgs),

    /// List configured remotes
    #[command(alias = "ls")]
    List,

    /// Remove a configured remote
    #[command(alias = "rm")]
    Remove(RemoteRemoveArgs),

    /// Enable or disable a remote without removing it
    Toggle(RemoteToggleArgs),
}

#[derive(Args)]
pub struct RemoteAddArgs {
    /// Where the remote daemon listens: `host:port` for a LAN or tailnet
    /// address (plain HTTP), a hostname (HTTPS), or a full URL. A `?token=`
    /// query, as `aoe serve --status` prints it, supplies the token.
    pub address: String,

    /// Short name used to select this remote. Defaults to the remote's
    /// hostname
    #[arg(long)]
    pub name: Option<String>,

    /// Bearer token the daemon prints at startup
    #[arg(long)]
    pub token: Option<String>,

    /// Passphrase for a daemon started with `--remote`. Exchanged once for a
    /// device-bound session; never stored.
    #[arg(long, env = "AOE_REMOTE_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// One-time pairing code shown in the remote's Remote Access view (R in
    /// its `aoe`). Used when no token or passphrase is given; prompted for on
    /// a terminal.
    #[arg(long, conflicts_with_all = ["token", "passphrase"])]
    pub code: Option<String>,

    /// Send credentials over plain HTTP to a non-loopback URL, for a daemon
    /// on a network you trust. Anyone on that network can read the token and
    /// session. Asked on a terminal when not given.
    #[arg(long)]
    pub insecure: bool,
}

#[derive(Args)]
pub struct RemoteRemoveArgs {
    pub name: String,
}

#[derive(Args)]
pub struct RemoteToggleArgs {
    pub name: String,

    /// Disable instead of enable
    #[arg(long)]
    pub off: bool,
}

pub async fn run(command: RemoteCommands) -> Result<()> {
    match command {
        RemoteCommands::Add(args) => add(args).await,
        RemoteCommands::List => list(),
        RemoteCommands::Remove(args) => remove(args),
        RemoteCommands::Toggle(args) => toggle(args),
    }
}

async fn add(args: RemoteAddArgs) -> Result<()> {
    if args
        .name
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        bail!("remote name must not be empty");
    }
    let address = remotes::parse_remote_address(&args.address)?;
    let (url, token) = token_from_url(&address, args.token.clone())?;
    let url = url.trim_end_matches('/').to_string();
    if args.code.is_some() && token.is_some() {
        bail!("the URL carries a token; pass either it or --code, not both");
    }
    let insecure = args.insecure
        || (url.starts_with("http://")
            && login::ensure_secure_transport(&url, false).is_err()
            && plaintext_consent(std::io::stdin().is_terminal(), || {
                eprint!(
                    "Plain HTTP: the code and session travel unencrypted on this network. \
                     Continue? [y/N] "
                );
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                Ok(line)
            })?);
    let args = RemoteAddArgs {
        token,
        insecure,
        ..args
    };
    // The same URL and transport rules every later poll applies, so an entry
    // that could never be used is refused now rather than stored.
    let plaintext_refused = |url: &str| {
        format!(
            "cannot use {url:?} as a remote; credentials need HTTPS, or pass --insecure \
             for a daemon on a trusted LAN"
        )
    };
    match DaemonClient::with_login(&url, args.token.as_deref(), None, args.insecure) {
        Err(DaemonClientError::InsecureBearerTransport) => bail!(plaintext_refused(&url)),
        other => other.with_context(|| format!("cannot use {url:?} as a remote"))?,
    };

    let mut entry = Remote {
        name: String::new(),
        url: url.clone(),
        enabled: true,
        token: args.token.clone(),
        session: None,
        binding: None,
        insecure: args.insecure,
    };

    let pairing = args.token.is_none() && args.passphrase.is_none();
    let mut no_login_wall = false;
    let mut server_name = None;
    if pairing {
        login::ensure_secure_transport(&url, args.insecure).map_err(|e| match e {
            LoginError::InsecureTransport => anyhow::anyhow!(plaintext_refused(&url)),
            other => anyhow::Error::new(other),
        })?;
        let code = match args.code.clone() {
            Some(code) => code,
            None => prompt_code(&url)?,
        };
        let binding = login::new_binding_secret().context("generate device binding secret")?;
        let paired = login::pair(&url, &code, &device_name(), &binding, args.insecure)
            .await
            .context("pairing failed")?;
        entry.session = Some(paired.credential.session);
        entry.binding = Some(paired.credential.binding);
        server_name = paired.server_name;
    } else if let Some(passphrase) = args.passphrase.as_deref() {
        let binding = login::new_binding_secret().context("generate device binding secret")?;
        match login::login(
            &url,
            args.token.as_deref(),
            passphrase,
            &binding,
            args.insecure,
        )
        .await
        {
            Ok(credentials) => {
                entry.session = Some(credentials.session);
                entry.binding = Some(credentials.binding);
            }
            // Either a token-only daemon or a wrong URL; the session read
            // below tells them apart.
            Err(LoginError::NotEnabled) => no_login_wall = true,
            Err(LoginError::InsecureTransport) => bail!(plaintext_refused(&url)),
            Err(e) => return Err(e).context("passphrase login failed"),
        }
    }

    verify(&entry).await?;
    if no_login_wall {
        println!("note: this daemon has no passphrase login; saved with the token only");
    }

    let mut registry = remotes::load()?;
    entry.name = match args.name.clone() {
        Some(name) => name,
        None => {
            let host = reqwest::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            registry.name_for(server_name.as_deref().unwrap_or(&host), &url)
        }
    };
    let name = entry.name.clone();
    let superseded = registry.remove_other_names_for(&name, &url);
    let replaced = registry.upsert(entry);
    remotes::save(&registry)?;
    for old in superseded {
        println!("Removed remote {old:?}, which pointed at the same address");
    }

    println!(
        "{} remote {:?} -> {}",
        if replaced { "Updated" } else { "Added" },
        name,
        url
    );
    if pairing {
        println!(
            "Paired as {:?}. Its sessions now list in `aoe`.",
            device_name()
        );
    } else if args.passphrase.is_none() {
        println!(
            "Its sessions now list in `aoe`. Re-add with --passphrase if it has a login wall."
        );
    }
    Ok(())
}

/// Whether to send credentials over plain HTTP without `--insecure`: only when
/// someone at a terminal says yes. A script gets the refusal instead.
fn plaintext_consent(
    interactive: bool,
    ask: impl FnOnce() -> std::io::Result<String>,
) -> Result<bool> {
    if !interactive {
        return Ok(false);
    }
    let answer = ask()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Read a pairing code from the terminal. Without one there is no credential
/// to add, so a non-interactive run is told how to pass it.
fn prompt_code(url: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "no credentials given; pass --code with the pairing code shown in the remote's \
             Remote Access view (R in its `aoe`), or --token"
        );
    }
    eprint!("Pairing code for {url} (shown under R in the remote's `aoe`): ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let code = line.trim().to_string();
    if code.is_empty() {
        bail!("no pairing code entered");
    }
    Ok(code)
}

/// How this machine names itself to a daemon it pairs with.
fn device_name() -> String {
    crate::util::hostname().unwrap_or_else(|| "aoe client".to_string())
}

/// Move a `?token=` query (as `aoe serve --status` and the TUI print it) into
/// the token, so the stored URL stays a base URL. A different `--token` is
/// refused rather than silently preferred.
fn token_from_url(raw: &str, token: Option<String>) -> Result<(String, Option<String>)> {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return Ok((raw.to_string(), token));
    };
    let (found, rest): (Vec<_>, Vec<_>) = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .partition(|(key, _)| key == "token");
    let Some((_, found)) = found.into_iter().next_back() else {
        return Ok((raw.to_string(), token));
    };
    if token.as_ref().is_some_and(|token| *token != found) {
        bail!("the URL carries a different token than --token; pass only one");
    }
    if rest.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(rest);
    }
    Ok((url.to_string(), Some(found)))
}

/// Read the session list with the entry's credentials, so a wrong base path,
/// a missing login or an unreachable host fails the add instead of every poll.
async fn verify(entry: &Remote) -> Result<()> {
    let client = entry.endpoint().daemon_client()?;
    match client.list_sessions(None).await {
        Ok(_) => Ok(()),
        Err(DaemonClientError::Status { status, .. }) if status == StatusCode::NOT_FOUND => bail!(
            "no aoe daemon API at {} (HTTP 404); check the URL and any base path",
            entry.url
        ),
        Err(DaemonClientError::Status { status, .. })
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) =>
        {
            bail!(
                "the daemon at {} refused these credentials (HTTP {}); check --token, and pass \
                 --passphrase if it has a login wall",
                entry.url,
                status.as_u16()
            )
        }
        Err(error @ DaemonClientError::Status { .. }) => {
            bail!("the daemon at {}: {}", entry.url, error.summary())
        }
        Err(e) => Err(e).with_context(|| format!("could not reach a daemon at {}", entry.url)),
    }
}

fn list() -> Result<()> {
    let registry = remotes::load()?;
    if registry.remotes().is_empty() {
        println!("No remotes configured. Add one with `aoe remote add <host:port>`.");
        return Ok(());
    }
    // Credentials are never printed, only whether they are present.
    println!("{:<16} {:<44} {:<8} AUTH", "NAME", "URL", "STATE");
    for remote in registry.remotes() {
        let auth = match (remote.token.is_some(), remote.has_login()) {
            (true, true) => "token+login",
            (true, false) => "token",
            (false, true) => "login",
            (false, false) => "none",
        };
        println!(
            "{:<16} {:<44} {:<8} {}{}",
            remote.name,
            remote.url,
            if remote.enabled { "enabled" } else { "off" },
            auth,
            if remote.insecure { " (insecure)" } else { "" }
        );
    }
    Ok(())
}

fn remove(args: RemoteRemoveArgs) -> Result<()> {
    let mut registry = remotes::load()?;
    if !registry.remove(&args.name) {
        bail!("no remote named {:?}", args.name);
    }
    remotes::save(&registry)?;
    println!("Removed remote {:?}", args.name);
    Ok(())
}

fn toggle(args: RemoteToggleArgs) -> Result<()> {
    let mut registry = remotes::load()?;
    let Some(existing) = registry.get(&args.name).cloned() else {
        bail!("no remote named {:?}", args.name);
    };
    let mut updated = existing;
    updated.enabled = !args.off;
    let enabled = updated.enabled;
    registry.upsert(updated);
    remotes::save(&registry)?;
    println!(
        "Remote {:?} is now {}",
        args.name,
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_from_url_moves_the_query_token_into_the_token() {
        let token = |t: &str| Some(t.to_string());
        for (raw, flag, expected) in [
            (
                "http://10.0.0.2:8081/?token=abc",
                None,
                ("http://10.0.0.2:8081/", token("abc")),
            ),
            (
                "http://10.0.0.2:8081/?token=abc",
                token("abc"),
                ("http://10.0.0.2:8081/", token("abc")),
            ),
            (
                "https://box.ts.net/?a=1&token=abc",
                None,
                ("https://box.ts.net/?a=1", token("abc")),
            ),
            (
                "https://box.ts.net",
                token("xyz"),
                ("https://box.ts.net", token("xyz")),
            ),
            ("not a url", None, ("not a url", None)),
        ] {
            let (url, found) = token_from_url(raw, flag).unwrap();
            assert_eq!((url.as_str(), found), (expected.0, expected.1), "{raw}");
        }
        assert!(token_from_url("http://10.0.0.2:8081/?token=abc", token("other")).is_err());
    }

    #[test]
    fn plaintext_needs_a_yes_from_someone_at_a_terminal() {
        for (interactive, answer, expected) in [
            (true, "y\n", true),
            (true, "YES\n", true),
            (true, "\n", false),
            (true, "no\n", false),
            (false, "y\n", false),
        ] {
            let consent = plaintext_consent(interactive, || Ok(answer.to_string())).unwrap();
            assert_eq!(consent, expected, "{interactive} {answer:?}");
        }
    }
}
