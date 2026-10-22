//! `aoe sandbox`: reclaim orphaned per-session agent stores and log the sandboxes in.

use std::io::Write;

use anyhow::Result;
use clap::{Args, Subcommand};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::migrations::progress::format_bytes;
use crate::session::config::container_config::{sandbox_login_status, SandboxLoginStatus};
use crate::session::config::SharedCredentialMode;
use crate::session::sandbox_login::code::LoginCode;
use crate::session::sandbox_login::helper::{LoginError, LoginHelper};
use crate::session::sandbox_store_reclaim as reclaim;

#[derive(Subcommand)]
pub enum SandboxCommands {
    /// Report per-session agent stores whose session no longer exists in any
    /// profile, and how much disk they hold. Reports only unless `--delete`
    /// is given: a store can hold a copy of that agent's credentials.
    Reclaim(ReclaimArgs),
    /// Log an agent's sandboxes in (sandbox.shared_credential = "separate_login"):
    /// prints a sign-in link, then reads the code shown after sign-in. The login
    /// belongs to one shared credential file per profile and tool: an agent with its
    /// own `session.agent_config_dir` entry (a wrapper agent) has
    /// its own file, so a session of profile X running tool T waiting for the login
    /// needs `aoe sandbox login -p X --tool T`. The file follows the command T
    /// launches (`agent_command_override`, else `custom_agents`, else the built-in).
    /// Only agents that declare a sandbox login have one; today that is Claude.
    Login(LoginArgs),
}

#[derive(Args)]
pub struct ReclaimArgs {
    /// Remove the reported stores instead of only naming them.
    #[arg(long)]
    pub delete: bool,
}

#[derive(Args)]
pub struct LoginArgs {
    /// The agent whose sandboxes the login is for: the tool name of the sessions
    /// waiting for it. Defaults to the profile's default tool when it has a sandbox
    /// login, else the only agent that has one.
    #[arg(long)]
    pub tool: Option<String>,
    /// The command the launch runs for `--tool` when a session overrides the profile's:
    /// the login lands in the file that launch is gated on. A session that waits for the
    /// login names it in the message it stops with; without `--command` the profile's
    /// command for the tool is used.
    #[arg(long, requires = "tool", value_name = "COMMAND")]
    pub command: Option<String>,
    /// Only report whether the shared credential file needs a login:
    /// exit 0 when ok, 2 when needed, 3 when not applicable (`host_seed` mode, a
    /// tool without a shared credential file, or a login that is needed on Apple
    /// Container, which cannot run the login helper).
    #[arg(long, conflicts_with = "force")]
    pub check: bool,
    /// Log in again even though the shared credential is usable.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(profile: &str, command: SandboxCommands) -> Result<()> {
    match command {
        SandboxCommands::Reclaim(args) => run_reclaim(args),
        SandboxCommands::Login(args) => run_sandbox_login(profile, args).await,
    }
}

#[tracing::instrument(target = "cli.sandbox", skip_all, fields(profile = %profile))]
async fn run_sandbox_login(profile: &str, args: LoginArgs) -> Result<()> {
    let tool = resolve_tool(profile, args.tool)?;
    if args.check {
        let (owned, tool, command) = (profile.to_string(), tool, args.command);
        let (line, code) =
            tokio::task::spawn_blocking(move || check_login(&owned, &tool, command.as_deref()))
                .await?;
        println!("{line}");
        return match code {
            0 => Ok(()),
            code => Err(super::Exit(code).into()),
        };
    }

    // Ctrl-C before the link is printed ends the process; the next start sweeps the container.
    eprintln!("Starting the {tool} login helper…");
    let helper = LoginHelper::start(profile, &tool, args.command.as_deref(), args.force)
        .await
        .map_err(login_failure)?;
    println!("Open this link and sign in:\n\n  {}\n", helper.url());
    let outcome = tokio::select! {
        outcome = read_and_submit(
            &*helper,
            tokio::io::BufReader::new(tokio::io::stdin()),
            std::io::stderr(),
        ) => outcome,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\ncancelling…");
            Err(LoginError::Cancelled)
        }
    };
    if let Err(error) = outcome {
        helper.cancel().await;
        return Err(login_failure(error));
    }
    println!("Logged in. The {tool} sandboxes use the new login from their next start.");
    Ok(())
}

/// The tool a login is for: `--tool`, else the profile's default tool when it has a sandbox
/// login, else the only agent that has one.
fn resolve_tool(profile: &str, tool: Option<String>) -> Result<String> {
    if let Some(tool) = tool {
        return Ok(tool);
    }
    crate::session::config::container_config::default_login_tool(profile).ok_or_else(|| {
        anyhow::anyhow!(
            "no default agent has a sandbox login: pass --tool (agents with one: {})",
            crate::session::config::container_config::login_capable_agents()
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The `--check` line and exit status for `tool` under `profile`.
fn check_login(profile: &str, tool: &str, command: Option<&str>) -> (String, i32) {
    let mode = crate::session::config::profile_config::resolve_config_or_warn(profile)
        .sandbox
        .shared_credential;
    check_report(sandbox_login_status(profile, tool, command), mode, tool)
}

/// The `--check` line and exit status for `status`. `NotApplicable` has two causes
/// that `mode` and `tool` tell apart.
fn check_report(
    status: SandboxLoginStatus,
    mode: SharedCredentialMode,
    tool: &str,
) -> (String, i32) {
    match status {
        SandboxLoginStatus::Ok { refresh_expires_at } => {
            let expiry = refresh_expires_at
                .and_then(|ms| i64::try_from(ms).ok())
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|at| {
                    format!(
                        " (refresh token expires {})",
                        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    )
                })
                .unwrap_or_default();
            (format!("sandbox login for {tool}: ok{expiry}"), 0)
        }
        SandboxLoginStatus::Needed => (format!("sandbox login for {tool}: needed"), 2),
        SandboxLoginStatus::Unsupported => (
            format!(
                "sandbox login for {tool}: not applicable ({})",
                crate::session::SandboxLoginUnsupported
            ),
            3,
        ),
        SandboxLoginStatus::NotApplicable if mode == SharedCredentialMode::SeparateLogin => (
            format!(
                "sandbox login for {tool}: not applicable ({tool} is unknown or declares no sandbox login)"
            ),
            3,
        ),
        SandboxLoginStatus::NotApplicable => (
            format!(
                "sandbox login for {tool}: not applicable (sandbox.shared_credential is \"host_seed\")"
            ),
            3,
        ),
    }
}

/// Where a login code goes; `LoginHelper` in production.
pub(crate) trait CodeSink {
    /// One code per login: any `Err` ends it.
    async fn submit_code(&self, code: LoginCode) -> Result<(), LoginError>;
    /// Resolves once the login ended without a code (timeout, cancel, client exit).
    async fn ended(&self) -> LoginError;
}

/// Prompts on `err` until a line parses as a code, then submits that one code.
/// EOF on `input` gives `Cancelled` without a submission.
async fn read_and_submit<R: AsyncBufRead + Unpin, W: Write>(
    sink: &impl CodeSink,
    mut input: R,
    mut err: W,
) -> Result<(), LoginError> {
    loop {
        write!(err, "Paste the code: ")
            .and_then(|()| err.flush())
            .map_err(|e| LoginError::Other(e.into()))?;
        let mut line = String::new();
        let read = tokio::select! {
            read = input.read_line(&mut line) => read,
            ended = sink.ended() => return Err(ended),
        };
        match read {
            Ok(0) => return Err(LoginError::Cancelled),
            Ok(_) => {}
            Err(e) => return Err(LoginError::Other(e.into())),
        }
        match LoginCode::parse(line) {
            Ok(code) => return sink.submit_code(code).await,
            Err(invalid) => {
                writeln!(err, "{invalid}").map_err(|e| LoginError::Other(e.into()))?;
            }
        }
    }
}

/// The error the command ends with; a rejected code needs a new link.
fn login_failure(error: LoginError) -> anyhow::Error {
    match error {
        LoginError::Failed { .. } => {
            anyhow::anyhow!("{error}\nRun aoe sandbox login again for a new link.")
        }
        error => error.into(),
    }
}

impl CodeSink for LoginHelper {
    async fn submit_code(&self, code: LoginCode) -> Result<(), LoginError> {
        LoginHelper::submit_code(self, code).await
    }

    async fn ended(&self) -> LoginError {
        LoginHelper::ended(self).await
    }
}

fn run_reclaim(args: ReclaimArgs) -> Result<()> {
    if !args.delete {
        let plan = reclaim::report()?;
        print_plan(&plan);
        if !plan.orphans.is_empty() {
            println!("\nRun `aoe sandbox reclaim --delete` to remove them.");
        }
        return Ok(());
    }

    let outcome = reclaim::reclaim()?;
    print_plan(&outcome.plan);
    println!();
    if outcome.removed.is_empty() {
        println!("Removed nothing.");
    } else {
        println!(
            "Removed {} store(s), freeing {}.",
            outcome.removed.len(),
            format_bytes(outcome.freed())
        );
    }
    for (path, reason) in &outcome.failures {
        eprintln!("aoe: kept {}: {reason}", path.display());
    }
    Ok(())
}

fn print_plan(plan: &reclaim::Plan) {
    println!(
        "Scanned {} store root(s) against {} session(s) across all profiles.",
        plan.roots.len(),
        plan.owners
    );
    for (path, reason) in &plan.preserved {
        println!("  preserved  {}  ({})", path.display(), reason.label());
    }
    if plan.orphans.is_empty() {
        println!("No orphaned agent stores.");
        return;
    }
    for orphan in &plan.orphans {
        println!(
            "  orphan     {}  ({})",
            orphan.path.display(),
            format_bytes(orphan.bytes)
        );
    }
    println!(
        "{} orphaned store(s), {} total.",
        plan.orphans.len(),
        format_bytes(plan.bytes())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    const DEADLINE: Duration = Duration::from_secs(10);

    #[test]
    fn sandbox_login_cli_check_exit_codes() {
        let cases = [
            (
                SandboxLoginStatus::Ok {
                    refresh_expires_at: None,
                },
                "sandbox login for claude: ok",
                0,
            ),
            (
                SandboxLoginStatus::Ok {
                    refresh_expires_at: Some(1_790_000_000_000),
                },
                "sandbox login for claude: ok (refresh token expires 2026-09-21T14:13:20Z)",
                0,
            ),
            (
                SandboxLoginStatus::Needed,
                "sandbox login for claude: needed",
                2,
            ),
            (
                SandboxLoginStatus::NotApplicable,
                "sandbox login for claude: not applicable (sandbox.shared_credential is \"host_seed\")",
                3,
            ),
            (
                SandboxLoginStatus::Unsupported,
                "sandbox login for claude: not applicable (sandbox.shared_credential is separate_login, which is not supported with Apple Container; use host_seed or Docker/Podman)",
                3,
            ),
        ];
        for (status, line, code) in cases {
            assert_eq!(
                check_report(status, SharedCredentialMode::HostSeed, "claude"),
                (line.to_string(), code),
                "{status:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_login_cli_check_names_the_cause_of_not_applicable() {
        let _home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        assert_eq!(
            check_login(&profile, "claude", None),
            (
                "sandbox login for claude: not applicable (sandbox.shared_credential is \"host_seed\")"
                    .to_string(),
                3
            )
        );

        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
        })
        .unwrap();
        for tool in ["claude-persnal", "codex"] {
            let (line, code) = check_login(&profile, tool, None);
            assert_eq!(code, 3, "{tool}");
            assert!(!line.contains("host_seed"), "{tool}: {line}");
            assert!(line.contains(tool), "{tool}: {line}");
            assert!(line.contains("declares no sandbox login"), "{tool}: {line}");
        }
        assert_eq!(
            check_login(&profile, "claude", None).1,
            2,
            "a real tool still needs the login"
        );

        crate::session::config::update_config(|c| {
            c.sandbox.container_runtime =
                crate::session::config::ContainerRuntimeName::AppleContainer;
        })
        .unwrap();
        assert_eq!(
            check_login(&profile, "claude", None),
            (
                "sandbox login for claude: not applicable (sandbox.shared_credential is separate_login, which is not supported with Apple Container; use host_seed or Docker/Podman)"
                    .to_string(),
                3
            )
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_login_cli_tool_defaults_to_the_profile_default_or_the_only_login_agent() {
        let _home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        let resolved = |tool: Option<&str>| resolve_tool(&profile, tool.map(str::to_string));

        assert_eq!(resolved(Some("codex")).unwrap(), "codex", "--tool wins");
        assert_eq!(
            resolved(None).unwrap(),
            "claude",
            "the only agent with a login"
        );

        crate::session::config::update_config(|c| {
            c.session.default_tool = Some("codex".to_string());
        })
        .unwrap();
        assert_eq!(
            resolved(None).unwrap(),
            "claude",
            "a default tool without a login is skipped"
        );

        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("claude-personal".to_string(), "claude".to_string());
            c.session
                .agent_detect_as
                .insert("claude-personal".to_string(), "claude".to_string());
            c.session.default_tool = Some("claude-personal".to_string());
        })
        .unwrap();
        assert_eq!(
            resolved(None).unwrap(),
            "claude-personal",
            "a wrapper default tool with a login is the target"
        );
    }

    #[derive(clap::Parser)]
    struct LoginCli {
        #[command(flatten)]
        args: LoginArgs,
    }

    fn parse_login(args: &[&str]) -> Result<LoginArgs, clap::Error> {
        use clap::Parser;
        LoginCli::try_parse_from(std::iter::once("login").chain(args.iter().copied()))
            .map(|cli| cli.args)
    }

    #[test]
    fn sandbox_login_cli_command_flag_parses_next_to_tool() {
        let args = parse_login(&["--tool", "personal", "--command", "claude --model x"]).unwrap();
        assert_eq!(args.tool.as_deref(), Some("personal"));
        assert_eq!(args.command.as_deref(), Some("claude --model x"));
        assert_eq!(
            parse_login(&["--tool", "personal"]).unwrap().command,
            None,
            "the profile's command is the default"
        );
        assert!(
            parse_login(&["--command", "claude"]).is_err(),
            "a command belongs to a tool"
        );
        assert!(parse_login(&["--tool", "personal", "--command", "claude", "--check"]).is_ok());
        assert!(parse_login(&[
            "--tool",
            "personal",
            "--command",
            "claude",
            "--check",
            "--force"
        ])
        .is_err());
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_login_cli_check_follows_the_command_of_a_session_override() {
        let _home = crate::session::test_support::isolate_app_dir();
        let profile = crate::session::config::effective_profile("");
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.codex-personal".to_string());
        })
        .unwrap();

        let (line, code) = check_login(&profile, "personal", None);
        assert_eq!(code, 3, "the profile's command is Codex: {line}");
        assert!(line.contains("declares no sandbox login"), "{line}");
        assert_eq!(
            check_login(&profile, "personal", Some("claude")),
            ("sandbox login for personal: needed".to_string(), 2)
        );
    }

    /// The recovery a refused launch prints parses with the real CLI definition, also for a
    /// profile, tool or command that starts with `-` (printed in the `--flag=value` form).
    #[test]
    fn sandbox_login_recovery_command_parses_with_the_cli_definition() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        let cases = [
            (None, "claude", None),
            (Some("work"), "claude", Some("claude --model x")),
            (Some("-x"), "claude", None),
            (Some("-x"), "-t", Some("-c --flag")),
            (Some("my work"), "personal", Some("claude 'quoted arg'")),
        ];
        for (profile, tool, command) in cases {
            let mut required = crate::session::test_support::login_required(tool);
            required.profile = profile.map(str::to_string);
            required.command = command.map(str::to_string);
            let line = required.login_command();
            let argv = shell_words::split(&line).unwrap();
            let cli = Cli::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("`{line}` does not parse: {e}"));
            assert_eq!(cli.profile.as_deref(), profile, "{line}");
            let Some(Commands::Sandbox {
                command: SandboxCommands::Login(args),
            }) = cli.command
            else {
                panic!("`{line}` is not a sandbox login");
            };
            assert_eq!(args.tool.as_deref(), Some(tool), "{line}");
            assert_eq!(args.command.as_deref(), command, "{line}");
        }
    }

    /// Records submitted codes; answers with `answer` and ends with `ended` (never when `None`).
    struct FakeSink {
        submitted: Mutex<Vec<String>>,
        answer: fn() -> Result<(), LoginError>,
        ended: Option<fn() -> LoginError>,
    }

    impl FakeSink {
        fn new(answer: fn() -> Result<(), LoginError>) -> Self {
            Self {
                submitted: Mutex::new(Vec::new()),
                answer,
                ended: None,
            }
        }

        fn submitted(&self) -> Vec<String> {
            self.submitted.lock().unwrap().clone()
        }
    }

    impl CodeSink for FakeSink {
        async fn submit_code(&self, code: LoginCode) -> Result<(), LoginError> {
            self.submitted
                .lock()
                .unwrap()
                .push(code.as_str().to_string());
            (self.answer)()
        }

        async fn ended(&self) -> LoginError {
            match self.ended {
                Some(ended) => ended(),
                None => std::future::pending().await,
            }
        }
    }

    async fn read(sink: &FakeSink, input: &str) -> (Result<(), LoginError>, String) {
        let mut err = Vec::new();
        let result =
            tokio::time::timeout(DEADLINE, read_and_submit(sink, input.as_bytes(), &mut err))
                .await
                .expect("deadline exceeded");
        (result, String::from_utf8(err).unwrap())
    }

    #[tokio::test]
    async fn sandbox_login_cli_reads_one_code() {
        let sink = FakeSink::new(|| Ok(()));
        let (result, err) = read(&sink, "has space\ngood\n").await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(sink.submitted(), ["good"]);
        let invalid = crate::session::sandbox_login::code::CodeError::InvalidCharacter.to_string();
        assert_eq!(
            err,
            format!("Paste the code: {invalid}\nPaste the code: "),
            "the code itself is never written"
        );

        let sink = FakeSink::new(|| {
            Err(LoginError::Failed {
                message: "Login failed: Request failed with status code 400 (exit 1)".into(),
            })
        });
        let (result, _) = read(&sink, "wrong-AAAA\nsecond-BBBB\n").await;
        assert_eq!(sink.submitted(), ["wrong-AAAA"], "one code per helper");
        let failure = login_failure(result.unwrap_err()).to_string();
        assert_eq!(
            failure,
            "the sandbox login failed:\nLogin failed: Request failed with status code 400 (exit 1)\n\
             Run aoe sandbox login again for a new link."
        );
        assert_eq!(
            login_failure(LoginError::Timeout).to_string(),
            "the sandbox login timed out"
        );

        let sink = FakeSink::new(|| Ok(()));
        let (result, err) = read(&sink, "").await;
        assert!(matches!(result, Err(LoginError::Cancelled)), "{result:?}");
        assert!(sink.submitted().is_empty());
        assert_eq!(err, "Paste the code: ");

        // The login ending while the terminal stays silent ends the wait.
        let mut sink = FakeSink::new(|| Ok(()));
        sink.ended = Some(|| LoginError::Timeout);
        let (_input, idle) = tokio::io::duplex(64);
        let mut err = Vec::new();
        let result = tokio::time::timeout(
            DEADLINE,
            read_and_submit(&sink, tokio::io::BufReader::new(idle), &mut err),
        )
        .await
        .expect("deadline exceeded");
        assert!(matches!(result, Err(LoginError::Timeout)), "{result:?}");
        assert!(sink.submitted().is_empty());
    }
}
