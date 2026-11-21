//! `aoe setup`: the default structured-view path, in one non-interactive run.
//!
//! Detection and the decision both come from `crate::acp::quickstart`, which the
//! first-run wizard also uses, so the CLI and the wizard cannot disagree about
//! the default agent.

use std::io::IsTerminal;

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::quickstart::{
    self, Detection, GatewayCredential, OTARI_QUICKSTART_URL, OTARI_SIGNUP_URL, OTARI_SUPPORT_URL,
};
use crate::session::config::{StructuredView, DEFAULT_ACP_AGENT};

#[derive(Args)]
pub struct SetupArgs {
    /// Never prompt. The bundled adapter is installed only with
    /// `--install-adapter` or an `acp.allow_agent_install` already set;
    /// otherwise the command that installs it is printed.
    #[arg(long)]
    quick: bool,
    /// Consent to installing the bundled `aoe-agent` adapter into the data dir
    /// (no global npm install, no sudo).
    #[arg(long)]
    install_adapter: bool,
    /// Default to this agent, which must already be on the machine, instead of
    /// the bundled one. Reuses the CLI you already have and its login. A rerun
    /// without this flag keeps the default it finds; passing it changes that
    /// default.
    #[arg(long, value_name = "AGENT")]
    agent: Option<String>,
}

#[tracing::instrument(target = "cli.setup", skip_all)]
pub async fn run(args: SetupArgs) -> Result<()> {
    let config = crate::session::config::Config::load_or_warn();
    let detection = Detection::detect_env();
    let app_dir = crate::session::get_app_dir().ok();

    let adapter_missing = app_dir.as_deref().is_some_and(|app_dir| {
        !crate::acp::adapters::installation_is_current(app_dir, quickstart::BUNDLED_AGENT)
    });
    let install_allowed = args.install_adapter
        || config.acp.allow_agent_install
        || (!args.quick && adapter_missing && prompt_install(quickstart::BUNDLED_AGENT));

    let mut plan = quickstart::plan(&detection, &config.acp, install_allowed);
    if let Some(requested) = &args.agent {
        if requested != quickstart::BUNDLED_AGENT
            && !detection
                .present_agents
                .iter()
                .any(|name| name == requested)
        {
            let present = if detection.present_agents.is_empty() {
                "none detected".to_string()
            } else {
                detection.present_agents.join(", ")
            };
            bail!(
                "`{requested}` is not an installed agent. Detected: {present}. \
                 Run `aoe agents` for the full list, or leave --agent off to use \
                 the bundled {agent}.",
                agent = quickstart::BUNDLED_AGENT
            );
        }
        plan.bundled = requested == quickstart::BUNDLED_AGENT;
        plan.agent = requested.clone();
        plan.install = (plan.bundled && adapter_missing && install_allowed)
            .then_some(quickstart::BUNDLED_AGENT);
        plan.fallback = None;
    }

    println!("aoe setup");
    println!("=========");
    println!(
        "Default agent: {} {}",
        plan.agent,
        if plan.bundled {
            "(bundled)"
        } else {
            "(already installed)"
        }
    );
    if let Some(reason) = plan.fallback {
        println!("Using it because {reason}.");
    }

    let mut adapter_ready = !plan.bundled || detection.bundled_installed;
    if let (Some(binary), Some(app_dir)) = (plan.install, app_dir.as_deref()) {
        adapter_ready = crate::cli::acp::install_adapters(app_dir, &[binary]).await;
        // An install can still leave the runtime short, and the success line
        // has to match what launch would do.
        adapter_ready &= quickstart::bundled_ready(Some(app_dir));
    }

    let mut written: Vec<(&'static str, String, &'static str)> = Vec::new();
    let result = crate::session::config::update_config(|config| {
        let changes = quickstart::changes(&config.acp, &plan);
        if let Some(agent) = changes.default_agent {
            config.acp.default_agent = agent.clone();
            written.push(("acp.default_agent", agent, DEFAULT_ACP_AGENT));
        }
        if changes.structured_view {
            config.acp.structured_view = StructuredView::Structured;
            written.push(("acp.structured_view", "structured".to_string(), "auto"));
        }
    });
    let write_error = match result {
        Ok(()) => None,
        Err(e) => {
            println!("Could not write the configuration: {e}");
            Some(e)
        }
    };
    // Recovery guidance still prints below; a failed write is returned at the
    // end so a script cannot read the run as configured.
    if write_error.is_none() {
        if written.is_empty() {
            println!("Configuration already set; nothing to change.");
        } else {
            println!();
            println!("Configured in {}:", crate::session::config_path_display());
            for (key, value, undo) in &written {
                println!("  {key} = \"{value}\"");
                println!("    undo: set {key} = \"{undo}\" in that file");
            }
        }
    }

    if plan.bundled && !adapter_ready {
        println!();
        println!(
            "The bundled adapter is not installed yet. Install it with:\n  \
             aoe acp doctor --fix --adapter {}",
            quickstart::BUNDLED_AGENT
        );
    }

    // Only the bundled agent needs the Otari key; an agent the user already runs
    // keeps its own login.
    let otari = plan.bundled && plan.needs_gateway_key;
    if otari {
        print_otari_guidance();
    }

    println!();
    // The bundled agent's default model routes straight to Anthropic, so its
    // session line always pins a gateway model id. The id is pinned per
    // session: the Daemon forwards only allowlisted provider variables, so an
    // exported AOE_AGENT_MODEL would be dropped.
    let own_gateway =
        plan.bundled && detection.gateway_credential == Some(GatewayCredential::Gateway) && !otari;
    if plan.bundled && otari {
        println!("Once the key is exported, start a session on an Otari model:");
    } else if plan.bundled && own_gateway {
        println!("Start a session, pinning a model id your gateway serves:");
    } else if plan.bundled {
        println!("Start a session on an Otari model:");
    } else if !adapter_ready {
        println!("Once the adapter is installed:");
    } else {
        println!("Start a structured session:");
    }
    match (plan.bundled, own_gateway) {
        (false, _) => println!("  aoe add . --agent {} --structured-view", plan.agent),
        (true, true) => println!(
            "  aoe add . --agent {} --structured-view --model openai-compatible:<vendor>/<model>",
            plan.agent
        ),
        (true, false) => println!(
            "  aoe add . --agent {} --structured-view --model {}",
            plan.agent,
            quickstart::OTARI_MODEL_EXAMPLE
        ),
    }

    if !plan.alternatives.is_empty() {
        println!();
        println!(
            "Already on this machine: {}. Default to one instead with:\n  \
             aoe setup --agent <name>",
            plan.alternatives.join(", ")
        );
    }

    match write_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Ask before an install. A non-tty caller (CI, a piped run) never installs
/// silently, so the answer defaults to no.
fn prompt_install(binary: &str) -> bool {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }
    print!(
        "Install the bundled {binary} adapter into the aoe data dir \
         (no global npm install)? [y/N] "
    );
    use std::io::Write;
    if std::io::stdout().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn print_otari_guidance() {
    println!();
    println!("Otari account (aoe-agent needs a provider key)");
    println!("  Otari is Mozilla AI's model gateway, and the key is all aoe-agent needs.");
    println!("  1. Create a free account (GitHub, Google or email):");
    println!("       {OTARI_SIGNUP_URL}");
    println!("  2. Create an API key in the dashboard.");
    println!("  3. Export it where the daemon runs, then re-run setup:");
    println!("       export OTARI_API_KEY=\"paste-the-key-here\"");
    println!("  Docs: {OTARI_QUICKSTART_URL}");
    println!("  Support: Mozilla AI Discord {OTARI_SUPPORT_URL}");
}
