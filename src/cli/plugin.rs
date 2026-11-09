//! `aoe plugin`: plugin management (list, info, enable, disable, install,

use std::io::{self, IsTerminal, Write};

use crate::acp::client::{HttpClient, HttpError};
use crate::daemon::{
    ApplyUpdateBody, InstallPreviewBody, PluginJobAccepted, PluginJobStatus, StartInstallBody,
};
use crate::plugin::install::{UiView, UpdatePreview};
use anyhow::{bail, Context, Result};
use clap::Subcommand;

#[derive(Debug, thiserror::Error)]
#[error("Remote plugin inspection is not supported; use the same Daemon's /api/plugins API, or run this command on its host without --daemon-url or AOE_DAEMON_URL")]
struct RemoteInspectionUnsupported;

#[derive(Subcommand)]
pub enum PluginCommands {
    /// List every known plugin with version, validation, and state
    List,
    /// Show one plugin's manifest details
    Info {
        /// Plugin id, e.g. `aoe.web`
        id: String,
    },
    /// Print the path of a plugin worker's most recent log
    Logs {
        /// Plugin id
        id: String,
    },
    /// Enable a plugin's contributions
    Enable {
        /// Plugin id
        id: String,
    },
    /// Disable a plugin; its settings stay on disk for re-enabling
    Disable {
        /// Plugin id
        id: String,
    },
    /// Install an external plugin from a `gh:owner/repo[@ref]` slug or a local
    /// directory. With no `@ref`, installs the repo's latest release; an
    /// explicit `@ref` installs unverified, un-audited code. Community plugins
    /// run at your own risk.
    Install {
        /// `gh:owner/repo` (latest release) or `gh:owner/repo@ref` (unverified)
        /// or a local directory path
        source: String,
        /// Grant all requested capabilities without prompting
        #[arg(long)]
        yes: bool,
        /// Install a local directory as a dev plugin: it may claim a reserved
        /// `aoe.*` / `agent-of-empires.*` id, is never featured, and is refused
        /// under `aoe serve --remote`
        #[arg(long)]
        dev: bool,
    },
    /// Update an installed external plugin from its recorded source and restart
    /// its worker in a running daemon. Prompts to re-approve capabilities if the
    /// update changes the capability set.
    Update {
        /// Plugin id
        id: String,
        /// Re-approve a changed capability set without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Uninstall an external plugin, removing its files and capability grant
    Uninstall {
        /// Plugin id
        id: String,
    },
    /// Observe an accepted plugin job without submitting another operation
    Job {
        /// Job id returned by install, update, or uninstall
        id: String,
    },
    /// Print the deterministic source tree hash for a plugin directory, the
    /// value a maintainer pins in the featured index
    Hash {
        /// Path to the plugin directory
        path: String,
    },
    /// Search GitHub's `aoe-plugin` topic for installable plugins
    Discover {
        /// Optional free-text term to narrow the search
        query: Option<String>,
    },
    /// List installed external plugins that have an update available
    Outdated,
}

pub async fn run(command: PluginCommands) -> Result<()> {
    if matches!(
        &command,
        PluginCommands::List
            | PluginCommands::Info { .. }
            | PluginCommands::Logs { .. }
            | PluginCommands::Outdated
    ) {
        anyhow::ensure!(
            std::env::var_os("AOE_DAEMON_URL")
                .as_deref()
                .is_none_or(super::runtime_read::env_url_is_unset),
            RemoteInspectionUnsupported,
        );
    }
    match command {
        PluginCommands::List => run_list(),
        PluginCommands::Info { id } => run_info(&id),
        PluginCommands::Logs { id } => run_logs(&id),
        PluginCommands::Enable { id } => run_set_enabled(&id, true).await,
        PluginCommands::Disable { id } => run_set_enabled(&id, false).await,
        PluginCommands::Install { source, yes, dev } => run_install(&source, yes, dev).await,
        PluginCommands::Update { id, yes } => run_update(&id, yes).await,
        PluginCommands::Uninstall { id } => run_uninstall(&id).await,
        PluginCommands::Job { id } => {
            let endpoint = crate::acp::client::require_daemon().await?;
            observe_job(&HttpClient::new(endpoint)?, &id).await
        }
        PluginCommands::Hash { path } => run_hash(&path),
        PluginCommands::Discover { query } => run_discover(query.as_deref()).await,
        PluginCommands::Outdated => run_outdated().await,
    }
}

fn run_hash(path: &str) -> Result<()> {
    let hash = crate::plugin::integrity::tree_hash(std::path::Path::new(path))?;
    println!("{hash}");
    Ok(())
}

fn run_logs(id: &str) -> Result<()> {
    let dir = crate::session::get_app_dir()?.join("plugin-workers");
    let Some(path) = crate::plugin::worker_logs::latest(&dir, id) else {
        anyhow::bail!("no worker log for plugin {id:?}; workers only run under `aoe serve`");
    };
    println!("{}", path.display());
    Ok(())
}

fn state_label(plugin: &crate::plugin::LoadedPlugin) -> &'static str {
    if !plugin.enabled {
        "disabled"
    } else if plugin.needs_reapproval() {
        "needs approval"
    } else {
        "enabled"
    }
}

fn run_list() -> Result<()> {
    let registry = crate::plugin::registry();
    if registry.all().is_empty() {
        println!("No plugins installed.");
    } else {
        println!("{:<20} {:<9} {:<12} STATE", "ID", "VERSION", "VALIDATION");
        for plugin in registry.all() {
            println!(
                "{:<20} {:<9} {:<12} {}",
                plugin.id(),
                plugin.manifest.version,
                plugin.validation.as_str(),
                state_label(plugin),
            );
        }
    }
    for err in registry.load_errors() {
        eprintln!("warning: {err}");
    }
    Ok(())
}

fn run_info(id: &str) -> Result<()> {
    let registry = crate::plugin::registry();
    let Some(plugin) = registry.get(id) else {
        anyhow::bail!("unknown plugin {id:?}; see `aoe plugin list`");
    };
    let m = &plugin.manifest;
    println!("{} ({})", m.name, m.id);
    println!("  version:    {}", m.version);
    println!("  validation: {}", plugin.validation.as_str());
    println!("  state:      {}", state_label(plugin));
    if let Some(source) = &plugin.source {
        println!("  source:     {source}");
    }
    if m.capabilities.is_empty() {
        println!("  caps:       none");
    } else {
        let caps: Vec<&str> = m.capabilities.iter().map(|c| c.as_str()).collect();
        println!(
            "  caps:       {} ({})",
            caps.join(", "),
            if plugin.granted {
                "granted"
            } else {
                "not granted"
            }
        );
    }
    if !m.ui.is_empty() {
        println!("  ui:");
        for u in &m.ui {
            println!("    - {} ({})", u.slot.as_str(), u.id);
        }
    }
    if !m.description.is_empty() {
        println!("  about:      {}", m.description);
    }
    if !m.keybinds.is_empty() {
        println!("  keybinds:");
        for kb in &m.keybinds {
            let note = match crate::tui::home::bindings::parse_chord(&kb.key) {
                Some(c) if crate::tui::home::bindings::core_shadows(&c) => "  (shadowed by core)",
                Some(_) => "",
                None => "  (invalid key, ignored)",
            };
            println!("    {} -> {}{note}", kb.key, kb.command);
        }
    }
    Ok(())
}

async fn run_set_enabled(id: &str, enabled: bool) -> Result<()> {
    let result = async {
        let endpoint = crate::acp::client::require_daemon().await?;
        crate::acp::client::HttpClient::new(endpoint)?
            .set_plugin_enabled(id, enabled)
            .await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if id == "aoe.web" && result.is_err() && crate::acp::client::discovery::discover_env().is_none()
    {
        result.map_err(crate::plugin::install::web_toggle_error)?;
    } else {
        result?;
    }
    println!("{} {id}.", if enabled { "Enabled" } else { "Disabled" });
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("stdin is not a terminal; re-run with --yes to approve this plugin");
    }
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn disclose(
    id: &str,
    capabilities: &[String],
    ui: &[UiView],
    build_steps: &[String],
    integrations: &[aoe_plugin_api::agent_integration::AgentIntegrationContribution],
) {
    println!("Plugin {id}:");
    for capability in capabilities {
        println!("  capability: {capability}");
    }
    for item in ui {
        println!("  UI: {} ({})", item.slot, item.id);
    }
    for command in build_steps {
        println!("  build (runs now as your user, unsandboxed): {command}");
    }
    for integration in integrations {
        println!(
            "  agent command (runs as your user, unsandboxed): {}: {}",
            integration.tool,
            integration
                .command
                .as_deref()
                .unwrap_or("No launch command declared"),
        );
    }
    println!("Only install plugins you trust. Workers and build steps are not contained by capability checks or OS sandboxing.");
}

async fn run_install(source: &str, yes: bool, dev: bool) -> Result<()> {
    use crate::plugin::source::PluginSource;

    let endpoint = crate::acp::client::require_daemon().await?;
    let source = match PluginSource::parse(source)? {
        PluginSource::Local(path) => {
            anyhow::ensure!(
                endpoint.unix_path().is_some(),
                "Local plugin sources and --dev require the local Daemon Unix socket; remote endpoints cannot read Client paths",
            );
            std::path::absolute(path)?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("plugin source path is not valid UTF-8"))?
        }
        PluginSource::Github { .. } => {
            anyhow::ensure!(!dev, "--dev requires a local plugin directory");
            source.trim().to_owned()
        }
    };
    let client = HttpClient::new(endpoint)?;
    let preview = InstallPreviewBody { source, dev };
    let consent = client.preview_plugin_install(&preview).await?;
    eprintln!("{}", consent.notice);
    if consent.unverified {
        println!("This is unverified, un-audited code, not covered by the featured index.");
        if !yes && !confirm("Continue?")? {
            bail!("install cancelled; the unverified source was not approved");
        }
    }
    disclose(
        &consent.id,
        &consent.capabilities,
        &consent.ui,
        &consent.build_steps,
        &consent.agent_integrations,
    );
    let needs_consent = !consent.capabilities.is_empty()
        || !consent.ui.is_empty()
        || !consent.build_steps.is_empty()
        || !consent.agent_integrations.is_empty();
    if needs_consent && !yes && !confirm("Grant them and install?")? {
        bail!("install cancelled; no capabilities were granted");
    }
    let accepted = submission(
        client
            .install_plugin(&StartInstallBody {
                source: preview.source,
                dev,
                expected_fingerprint: consent.fingerprint,
            })
            .await,
    )?;
    observe_job(&client, &accepted.job_id).await
}

async fn run_update(id: &str, yes: bool) -> Result<()> {
    let endpoint = crate::acp::client::require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let preview = match client.preview_plugin_update(id).await {
        Ok(preview) => preview,
        Err(error @ HttpError::Daemon(crate::daemon::DaemonClientError::AuthenticatedDecode)) => {
            return Err(error).context(
                "Plugin preview response is incompatible or malformed; no update was submitted. Check the Client and Daemon versions, then upgrade and restart the Daemon on its host with `aoe serve --restart`, or restart its foreground process or service manager",
            );
        }
        Err(error) => return Err(error.into()),
    };
    let fingerprint = match preview {
        UpdatePreview::NoUpdate {
            fingerprint,
            notice,
        }
        | UpdatePreview::SafeUpdate {
            fingerprint,
            notice,
            ..
        } => {
            eprintln!("{notice}");
            fingerprint
        }
        UpdatePreview::ConsentRequired {
            consent, notice, ..
        } => {
            eprintln!("{notice}");
            println!(
                "Update {id}: {} -> {}",
                consent.from_version, consent.to_version
            );
            if let Some(change) = &consent.runtime_change {
                println!("  runtime: {change}");
            }
            if consent.trust_downgrade {
                println!("  warning: this update is no longer featured");
            }
            disclose(
                id,
                &consent.new_capabilities,
                &consent.ui,
                &consent.build_steps,
                &consent.agent_integrations,
            );
            if !yes && !confirm("Approve this update?")? {
                bail!("update cancelled; the installed version is unchanged");
            }
            consent.fingerprint
        }
    };
    let accepted = submission(
        client
            .update_plugin(
                id,
                &ApplyUpdateBody {
                    expected_fingerprint: Some(fingerprint),
                },
            )
            .await,
    )?;
    observe_job(&client, &accepted.job_id).await
}

async fn run_uninstall(id: &str) -> Result<()> {
    let endpoint = crate::acp::client::require_daemon().await?;
    let client = HttpClient::new(endpoint)?;
    let accepted = submission(client.uninstall_plugin(id).await)?;
    observe_job(&client, &accepted.job_id).await
}

fn submission(
    result: std::result::Result<PluginJobAccepted, HttpError>,
) -> Result<PluginJobAccepted> {
    match result {
        Ok(accepted) => Ok(accepted),
        Err(error) if matches!(&error,
            HttpError::Unauthorized | HttpError::ReadOnly | HttpError::LocalPluginSourceRequired
                | HttpError::PluginJobActive
                | HttpError::PluginAlreadyInstalled
                | HttpError::Daemon(crate::daemon::DaemonClientError::InvalidPathSegment)
        ) || matches!(&error, HttpError::Server { status, .. }
            if status.is_client_error() && *status != reqwest::StatusCode::REQUEST_TIMEOUT) => Err(error.into()),
        Err(error) => Err(error).context(
            "Plugin operation outcome unknown; no operation was resubmitted. Inspect GET /api/plugins on the same Daemon before deciding whether to retry",
        ),
    }
}

async fn observe_job(client: &HttpClient, id: &str) -> Result<()> {
    eprintln!("Plugin job {id}; resume observation with `aoe plugin job {id}` on the same Daemon endpoint");
    let observe = async {
        let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            poll.tick().await;
            let response = client.plugin_job(id).await.with_context(|| format!(
                "Plugin job {id} outcome unknown; run `aoe plugin job {id}` on the same Daemon. No operation was resubmitted",
            ))?;
            match response.job.status {
                PluginJobStatus::Running => {}
                PluginJobStatus::Succeeded => {
                    if !response.log.tail.is_empty() {
                        println!("{}", response.log.tail);
                    }
                    println!("Plugin job {id} succeeded.");
                    return Ok(());
                }
                PluginJobStatus::Failed { error } => {
                    if !response.log.tail.is_empty() {
                        eprintln!("{}", response.log.tail);
                    }
                    if !response.log.path.is_empty() {
                        eprintln!("Full log on Daemon host: {}", response.log.path);
                    }
                    bail!("Plugin job {id} failed: {error}");
                }
            }
        }
    };
    tokio::select! {
        result = observe => result,
        _ = tokio::signal::ctrl_c() => bail!("Observation stopped; job {id} outcome unknown. Run `aoe plugin job {id}` on the same Daemon; no operation was resubmitted"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30 * 60)) => bail!("Observation timed out; job {id} outcome unknown. Run `aoe plugin job {id}` on the same Daemon; no operation was resubmitted"),
    }
}

async fn run_discover(query: Option<&str>) -> Result<()> {
    let results = crate::plugin::discover::discover(query).await?;
    if results.is_empty() {
        println!("No plugins found on the `aoe-plugin` topic.");
        return Ok(());
    }
    println!("{:<11} {:<6} {:<32} ABOUT", "BADGE", "STARS", "SOURCE");
    for r in &results {
        let about = r.description.as_deref().unwrap_or("");
        println!(
            "{:<11} {:<6} {:<32} {}",
            r.badge.as_str(),
            r.stars,
            r.slug,
            about
        );
    }
    println!("\nInstall with:\n  aoe plugin install <source>");
    Ok(())
}

async fn run_outdated() -> Result<()> {
    let statuses = crate::plugin::update_check::outdated().await;
    if statuses.is_empty() {
        println!("No external plugins installed.");
        return Ok(());
    }
    let mut any_outdated = false;
    for s in &statuses {
        if let Some(err) = &s.error {
            println!("error         {:<20} {}", s.id, err);
        } else if s.needs_update {
            any_outdated = true;
            let available = s.available.as_deref().unwrap_or("modified");
            println!("needs update  {:<20} {} -> {}", s.id, s.current, available);
        } else {
            println!("up to date    {:<20} {}", s.id, s.current);
        }
    }
    if any_outdated {
        println!("\nUpdate with:\n  aoe plugin update <id>");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    #[serial_test::serial]
    async fn uncertain_plugin_operations_are_not_resubmitted() {
        let _home = crate::session::test_support::isolate_app_dir();
        for lost_reply in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mutations = calls.clone();
            let router = Router::new()
                .route(
                    "/api/sessions",
                    get(|| async { Json(serde_json::json!([])) }),
                )
                .route(
                    "/api/plugins/acme.fixture/uninstall",
                    post(move || {
                        let calls = mutations.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            if lost_reply {
                                (StatusCode::ACCEPTED, "{")
                            } else {
                                (StatusCode::ACCEPTED, "{\"job_id\":\"accepted-fixture\"}")
                            }
                        }
                    }),
                )
                .route(
                    "/api/plugins/jobs/accepted-fixture",
                    get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let serve = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let _env = crate::session::test_support::EnvGuard::set(&[
                ("AOE_DAEMON_URL", format!("http://{address}")),
                ("AOE_DAEMON_TOKEN", String::new()),
                ("AOE_DAEMON_PASSPHRASE", String::new()),
            ]);
            let error = run_uninstall("acme.fixture").await.unwrap_err();
            assert!(
                format!("{error:#}").contains("outcome unknown"),
                "{error:#}"
            );
            if !lost_reply {
                assert!(error
                    .to_string()
                    .contains("aoe plugin job accepted-fixture"));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            serve.abort();
            let _ = serve.await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn older_job_logs_preserve_running_and_terminal_outcomes() {
        let _home = crate::session::test_support::isolate_app_dir();
        for failed in [false, true] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            let router = Router::new()
                .route("/api/sessions", get(|| async { Json(serde_json::json!([])) }))
                .route("/api/plugins/jobs/legacy-job", get(move || {
                    let polls = observed.clone();
                    async move {
                        let running = polls.fetch_add(1, Ordering::SeqCst) == 0;
                        let status = if running {
                            serde_json::json!({"state":"running"})
                        } else if failed {
                            serde_json::json!({"state":"failed","error":"legacy-build-failure"})
                        } else {
                            serde_json::json!({"state":"succeeded"})
                        };
                        Json(serde_json::json!({
                            "job": {
                                "id":"legacy-job", "kind":"update", "target":"acme.legacy",
                                "status":status, "started_at":1, "finished_at":if running {None} else {Some(2)},
                            },
                            "log": {
                                "exists":true, "tail":"legacy-build-output",
                                "lines_returned":1, "truncated":false,
                            },
                        }))
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let _endpoint = crate::session::test_support::EnvGuard::set(&[
                ("AOE_DAEMON_URL", format!("http://{address}")),
                ("AOE_DAEMON_TOKEN", String::new()),
                ("AOE_DAEMON_PASSPHRASE", String::new()),
            ]);
            let endpoint = crate::acp::client::require_daemon().await.unwrap();
            let client = HttpClient::new(endpoint).unwrap();
            let result = observe_job(&client, "legacy-job").await;
            server.abort();
            let _ = server.await;
            if failed {
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains("legacy-build-failure"), "{error}");
                assert!(!error.contains("outcome unknown"), "{error}");
            } else {
                result.unwrap();
            }
            assert_eq!(polls.load(Ordering::SeqCst), 2);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn inspection_and_preview_keep_endpoint_and_approval_boundaries() {
        use crate::session::test_support::EnvGuard;
        use std::os::unix::ffi::OsStringExt;

        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _home = crate::session::test_support::isolate_app_dir();
        let _credentials = EnvGuard::unset(&[
            "AOE_DAEMON_URL",
            "AOE_DAEMON_TOKEN",
            "AOE_DAEMON_PASSPHRASE",
        ]);
        crate::plugin::reload_registry();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("file"), b"local source").unwrap();
        for value in [
            std::ffi::OsString::from("http://127.0.0.1:1"),
            std::ffi::OsString::from_vec(vec![0xff]),
        ] {
            let _endpoint = EnvGuard::set(&[("AOE_DAEMON_URL", &value)]);
            for command in [
                PluginCommands::List,
                PluginCommands::Info {
                    id: "acme.local".into(),
                },
                PluginCommands::Logs {
                    id: "acme.local".into(),
                },
                PluginCommands::Outdated,
            ] {
                let error = run(command).await.unwrap_err();
                assert!(error
                    .downcast_ref::<RemoteInspectionUnsupported>()
                    .is_some());
            }
            run(PluginCommands::Hash {
                path: source.path().to_str().unwrap().into(),
            })
            .await
            .unwrap();
        }
        for value in ["", " \t "] {
            let _endpoint = EnvGuard::set(&[("AOE_DAEMON_URL", value)]);
            run(PluginCommands::List).await.unwrap();
        }

        let changelog = serde_json::json!({
            "entries": [], "truncated": false,
            "unavailable_reason": null, "more_url": null,
        });
        for preview in [
            serde_json::json!({
                "kind": "safe_update", "to_version": "2.0.0",
                "fingerprint": "fixture", "changelog": changelog,
            }),
            serde_json::json!({
                "kind": "consent_required", "dismissed": false,
                "consent": {
                    "id": "acme.old", "from_version": "1.0.0", "to_version": "2.0.0",
                    "prior_capabilities": [], "new_capabilities": ["net"],
                    "added_capabilities": ["net"], "removed_capabilities": [],
                    "ui": [], "build_steps": [], "agent_integrations": [],
                    "runtime_change": null, "trust_downgrade": false,
                    "fingerprint": "fixture", "stays_active_if_declined": true,
                    "changelog": changelog,
                },
            }),
        ] {
            let applies = Arc::new(AtomicUsize::new(0));
            let mutations = applies.clone();
            let preview = Arc::new(preview);
            let router = Router::new()
                .route(
                    "/api/sessions",
                    get(|| async { Json(serde_json::json!([])) }),
                )
                .route(
                    "/api/plugins/acme.old/update/preview",
                    get(move || {
                        let preview = preview.clone();
                        async move { Json(preview) }
                    }),
                )
                .route(
                    "/api/plugins/acme.old/update/apply",
                    post(move || {
                        let mutations = mutations.clone();
                        async move {
                            mutations.fetch_add(1, Ordering::SeqCst);
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let _endpoint = EnvGuard::set(&[("AOE_DAEMON_URL", format!("http://{address}"))]);
            let result = run_update("acme.old", true).await;
            server.abort();
            let _ = server.await;
            let error = result.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<HttpError>(),
                Some(HttpError::Daemon(
                    crate::daemon::DaemonClientError::AuthenticatedDecode
                ))
            ));
            assert_eq!(applies.load(Ordering::SeqCst), 0);
        }

        let refusal = submission(Err(HttpError::PluginAlreadyInstalled))
            .err()
            .unwrap();
        assert!(matches!(
            refusal.chain().next().unwrap().downcast_ref::<HttpError>(),
            Some(HttpError::PluginAlreadyInstalled)
        ));
        let unknown = submission(Err(HttpError::PluginPreviewFailed))
            .err()
            .unwrap();
        assert!(unknown
            .chain()
            .next()
            .unwrap()
            .downcast_ref::<HttpError>()
            .is_none());
    }
}
