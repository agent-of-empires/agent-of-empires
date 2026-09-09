//! xtask - Development tasks for agent-of-empires

use clap::{Args, CommandFactory, Parser, Subcommand};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Development tasks for agent-of-empires")]
struct Xtask {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate CLI documentation from clap definitions
    GenDocs,
    /// Check that contrib skill files reference valid CLI commands
    CheckSkill,
    /// Run the web dashboard backend and Vite dev server together (Ctrl-C stops both)
    Dev(DevArgs),
}

#[derive(Args)]
struct DevArgs {
    /// Port for the `aoe serve` backend (matches the debug-build default)
    #[arg(long, default_value_t = 8081)]
    serve_port: u16,
    /// Port for the Vite dev server
    #[arg(long, default_value_t = 5173)]
    web_port: u16,
    /// Interface the Vite dev server binds to. Defaults to loopback; pass
    /// `0.0.0.0` to reach the dashboard from other devices on the network. The
    /// backend stays on 127.0.0.1 and is reached through Vite's proxy.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Watch `src/**`, `Cargo.toml`, and `Cargo.lock`; on change rebuild and
    /// restart `aoe serve` (Vite stays up). Unix-only, same as the base command.
    #[arg(long)]
    watch: bool,
}

fn main() {
    let args = Xtask::parse();
    match args.command {
        Commands::GenDocs => generate_cli_docs(),
        Commands::CheckSkill => check_skill(),
        Commands::Dev(dev) => run_dev(dev),
    }
}

#[cfg(not(unix))]
fn run_dev(_args: DevArgs) {
    eprintln!("`cargo xtask dev` is unix-only (it relies on POSIX process groups).");
    std::process::exit(1);
}

/// Build the dashboard-enabled debug binary. Returns whether the build succeeded so
/// the watch loop can keep the old backend running on a failed rebuild.
#[cfg(unix)]
fn build_web() -> bool {
    use std::process::Command;
    eprintln!("[xtask dev] building aoe --features web...");
    Command::new("cargo")
        .args(["build", "--features", "web"])
        .status()
        .map(|s| s.success())
        .unwrap_or_else(|e| {
            eprintln!("[xtask dev] failed to run cargo build: {e}");
            false
        })
}

/// Whether a bind host keeps the dev servers off the network. Anything else
/// (notably `0.0.0.0`) exposes them to other devices and warrants a warning.
#[cfg(unix)]
fn host_is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
}

#[cfg(unix)]
fn child_exited(child: &mut std::process::Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

/// SIGTERM a child's process group, wait out a grace period, then SIGKILL the
/// group if it is still alive. Reaps the child either way.
#[cfg(unix)]
fn terminate_group(child: &mut std::process::Child, grace: std::time::Duration) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};
    if child_exited(child) {
        let _ = child.wait();
        return;
    }
    let pid = Pid::from_raw(child.id() as i32);
    let _ = killpg(pid, Signal::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child_exited(child) {
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = killpg(pid, Signal::SIGKILL);
    let _ = child.wait();
}

/// Wait until the backend port is bindable again before respawning, so a restart
/// does not race the old listener and fail with "address already in use".
#[cfg(unix)]
fn wait_for_port(port: u16, timeout: std::time::Duration) {
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("[xtask dev] port {port} still busy after waiting; respawning anyway");
}

/// Whether a changed path should trigger a backend rebuild: any `.rs` file, or
/// the root `Cargo.toml` / `Cargo.lock`. The watch scope (src/ recursively plus
/// the project root non-recursively) already excludes target/ and node_modules,
/// so a plain extension and file-name check is enough.
#[cfg(unix)]
fn is_watch_relevant(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) == Some("rs") {
        return true;
    }
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("Cargo.toml") | Some("Cargo.lock")
    )
}

/// Build the dashboard-enabled binary, then run it alongside the Vite dev server.
/// Vite proxies `/api` and the AoE `/sessions/*` WebSocket relays to the
/// backend via the `VITE_PROXY` env var it already honors. Each child runs in
/// its own process group so a single Ctrl-C tears the whole tree down (npm
/// spawns vite, vite may spawn esbuild) with no orphans.
///
/// With `--watch`, edits under `src/**` (plus `Cargo.toml` / `Cargo.lock`)
/// rebuild the backend and restart `aoe serve`; the Vite child is left running
/// so frontend HMR and the browser session survive the backend bounce.
#[cfg(unix)]
fn run_dev(args: DevArgs) {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // Build up front so build output doesn't interleave with Vite's startup
    // and a broken build fails fast before either server comes up.
    if !build_web() {
        std::process::exit(1);
    }

    // Honor CARGO_TARGET_DIR; cargo wrote the debug binary under it.
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let bin = Path::new(&target_dir).join("debug").join("aoe");

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::SeqCst))
            .expect("failed to install Ctrl-C handler");
    }

    // Detach stdin from both children: each runs in its own (background)
    // process group, so a TTY-driven raw-mode setup (Vite installs keypress
    // shortcuts when stdin is a TTY) would raise SIGTTOU and suspend the
    // child. Shutdown is driven by signals here, not per-server keystrokes,
    // so neither child needs the terminal.
    let serve_port = args.serve_port;
    // The backend always stays on loopback: remote devices reach the dashboard
    // through Vite, which proxies `/api` and the AoE `/sessions/*` WebSocket
    // relays to the backend over 127.0.0.1. Binding it to a public interface
    // would also trip `aoe serve`'s refusal to run `--no-auth` off-loopback
    // without a proxy.
    let host = args.host.clone();
    let spawn_serve = || -> Child {
        Command::new(&bin)
            .args(["serve", "--no-auth", "--port", &serve_port.to_string()])
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("failed to spawn `aoe serve`")
    };

    // Clear any lingering serve already bound to the dev namespace before we
    // spawn ours. An unclean prior `xtask dev` exit (or a stray dev daemon)
    // leaves a serve PID file that makes the fresh foreground serve refuse to
    // start with "already running", which then tears Vite down. Best-effort:
    // ignore the "no daemon running" case and wait for the port to free up.
    {
        let stopped = Command::new(&bin)
            .args(["serve", "--stop"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if stopped {
            eprintln!("[xtask dev] stopped a pre-existing dev `aoe serve` on :{serve_port}");
            wait_for_port(serve_port, Duration::from_secs(5));
        }
    }

    // Tracked as an Option so a backend that exits under --watch can be marked
    // dead and respawned on the next rebuild without tearing down Vite.
    let mut serve: Option<Child> = Some(spawn_serve());

    let mut vite = match Command::new("npm")
        .args([
            "--prefix",
            "web",
            "run",
            "dev",
            "--",
            "--port",
            &args.web_port.to_string(),
            "--host",
            &host,
        ])
        .env(
            "VITE_PROXY",
            format!("http://127.0.0.1:{}", args.serve_port),
        )
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            // serve is already up; tear its group down before bailing so we
            // don't orphan a backend on the serve port.
            eprintln!("[xtask dev] failed to spawn `npm run dev`: {e}");
            if let Some(mut serve) = serve.take() {
                terminate_group(&mut serve, Duration::from_secs(2));
            }
            std::process::exit(1);
        }
    };

    eprintln!(
        "[xtask dev] aoe serve on :{} | open http://localhost:{}{}",
        args.serve_port,
        args.web_port,
        if args.watch {
            " | watching src for changes"
        } else {
            ""
        }
    );
    if !host_is_loopback(&host) {
        eprintln!(
            "[xtask dev] WARNING: Vite is bound to {host}:{} and reachable from your \
             network. The proxied backend runs with --no-auth, so anyone who can reach \
             this port can control your agent sessions.",
            args.web_port
        );
    }

    // Watch src/** plus the root Cargo.toml/Cargo.lock when --watch is set. The
    // watcher must stay bound for the loop's lifetime; dropping it ends delivery.
    let (watch_tx, watch_rx) = std::sync::mpsc::channel::<()>();
    let _watcher = if args.watch {
        use notify::{RecursiveMode, Watcher};
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if event.paths.iter().any(|p| is_watch_relevant(p)) {
                    let _ = watch_tx.send(());
                }
            }
        })
        .expect("failed to create file watcher");
        // src/ recursively for .rs edits; the project root non-recursively so an
        // editor's atomic-save rename-replace of Cargo.toml/Cargo.lock is caught
        // (a direct file watch would detach when the inode is swapped).
        watcher
            .watch(Path::new("src"), RecursiveMode::Recursive)
            .expect("failed to watch src/");
        watcher
            .watch(Path::new("."), RecursiveMode::NonRecursive)
            .expect("failed to watch project root");
        Some(watcher)
    } else {
        drop(watch_tx);
        None
    };

    // Trailing debounce: the first change arms a deadline; rapid follow-up saves
    // (rustfmt, editor temp-file dances) collapse into a single rebuild.
    let debounce = Duration::from_millis(300);
    let mut rebuild_at: Option<Instant> = None;

    // Supervise: stop on Ctrl-C, on Vite exiting, or (without --watch) on the
    // backend exiting. Under --watch, rebuild and restart the backend on change.
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if child_exited(&mut vite) {
            eprintln!("[xtask dev] vite exited; stopping `aoe serve`");
            break;
        }
        if let Some(child) = serve.as_mut() {
            if child_exited(child) {
                if args.watch {
                    eprintln!("[xtask dev] `aoe serve` exited; waiting for a change to rebuild");
                    let _ = child.wait();
                    serve = None;
                } else {
                    eprintln!("[xtask dev] `aoe serve` exited; stopping vite");
                    break;
                }
            }
        }

        if args.watch {
            let mut saw_change = false;
            while watch_rx.try_recv().is_ok() {
                saw_change = true;
            }
            if saw_change {
                rebuild_at = Some(Instant::now() + debounce);
            }
            if let Some(at) = rebuild_at {
                if Instant::now() >= at {
                    rebuild_at = None;
                    eprintln!("[xtask dev] change detected; rebuilding aoe...");
                    if build_web() {
                        if let Some(mut old) = serve.take() {
                            terminate_group(&mut old, Duration::from_secs(2));
                        }
                        wait_for_port(serve_port, Duration::from_secs(5));
                        serve = Some(spawn_serve());
                        eprintln!("[xtask dev] aoe serve restarted on :{serve_port}");
                    } else {
                        eprintln!("[xtask dev] build failed; keeping the running backend");
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    // Signal each live process group: SIGTERM, brief grace, then SIGKILL so the
    // ports are always freed even if a child ignores the term.
    terminate_group(&mut vite, Duration::from_secs(2));
    if let Some(mut child) = serve.take() {
        terminate_group(&mut child, Duration::from_secs(2));
    }
}

fn generate_cli_docs() {
    let markdown = clap_markdown::help_markdown::<agent_of_empires::cli::Cli>();

    let docs_dir = Path::new("docs/cli");
    fs::create_dir_all(docs_dir).expect("Failed to create docs/cli directory");

    let output_path = docs_dir.join("reference.md");
    fs::write(&output_path, markdown).expect("Failed to write CLI reference");

    println!("Generated CLI documentation at {}", output_path.display());
}

/// The clap command tree, flattened into the three questions the skill check
/// asks of a command path it read out of a skill file.
#[derive(Default)]
struct CliTree {
    /// Canonical `aoe <path>` subcommand paths.
    commands: BTreeSet<String>,
    /// Paths that take a subcommand of their own, so a word after one of them
    /// is a subcommand name. After any other path it is a positional argument.
    parents: BTreeSet<String>,
    /// Paths reachable through a `#[command(alias = ...)]`. Accepted as input
    /// but kept out of the advisory: `aoe ls` in a skill file is not a
    /// documentation gap for `aoe list`, and aliases are absent from
    /// `docs/cli/reference.md` on purpose.
    aliases: BTreeSet<String>,
}

impl CliTree {
    fn from_command(cmd: &clap::Command) -> Self {
        let mut tree = Self::default();
        tree.walk(cmd, "", false);
        tree
    }

    /// Walks each spelling of each command, so a subcommand is recorded under
    /// its parent's aliases as well as its parent's name. Recursing only under
    /// the canonical name leaves `<alias> <sub>` in no set at all, and a parent
    /// path followed by an unknown word reports as nonexistent: a red check on
    /// correct documentation.
    ///
    /// `aliased` tracks whether any segment so far was an alias. One anywhere
    /// in the path keeps the whole path out of the advisory, since `aoe grp
    /// create` is not a documentation gap for `aoe group create`.
    fn walk(&mut self, cmd: &clap::Command, prefix: &str, aliased: bool) {
        for sub in cmd.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            let join = |name: &str| {
                if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix} {name}")
                }
            };
            let spellings = std::iter::once((sub.get_name(), false))
                .chain(sub.get_all_aliases().map(|alias| (alias, true)));
            for (name, is_alias) in spellings {
                let path = join(name);
                let aliased = aliased || is_alias;
                if sub.has_subcommands() {
                    self.parents.insert(path.clone());
                }
                if aliased {
                    self.aliases.insert(path.clone());
                } else {
                    self.commands.insert(path.clone());
                }
                self.walk(sub, &path, aliased);
            }
        }
    }

    fn is_known(&self, path: &str) -> bool {
        self.commands.contains(path) || self.aliases.contains(path)
    }
}

/// The spans of a markdown file where `aoe` is a command rather than the
/// ordinary English word it also is in these files: fenced code blocks, and
/// inline backtick spans in prose.
///
/// Spans are line-local, which is also what keeps the frontmatter's
/// `name: aoe` from reading as an invocation of whatever the next line
/// starts with.
fn code_spans(content: &str) -> Vec<&str> {
    // Only a shell fence holds commands. A `json` or `text` fence is data, and
    // the `#` truncation below would read a payload as a shell comment.
    fn is_shell_fence(info: &str) -> bool {
        matches!(
            info.trim(),
            "" | "sh" | "bash" | "shell" | "zsh" | "console" | "shell-session"
        )
    }

    let mut spans = Vec::new();
    // `Some(is_shell)` while inside a fence. Tracking "inside" separately from
    // "is shell" is what keeps a `json` fence's closing marker from reading as
    // the opening of a shell one and inverting every fence after it.
    let mut fence: Option<bool> = None;
    for line in content.lines() {
        if let Some(info) = line.trim_start().strip_prefix("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(is_shell_fence(info)),
            };
            continue;
        }
        if let Some(is_shell) = fence {
            if !is_shell {
                continue;
            }
            // Truncated at `#`, which is a comment in every shell fence these
            // files use. Prose naming a command is common there and is not an
            // invocation; dropping the tail can only under-check, never
            // redden a documentation edit.
            spans.push(line.split('#').next().unwrap_or(line));
            continue;
        }
        // An odd part count means the backticks pair up. Otherwise the spans
        // on this line cannot be told from its prose, so it is skipped.
        let parts: Vec<&str> = line.split('`').collect();
        if parts.len() % 2 == 1 {
            spans.extend(parts.iter().skip(1).step_by(2));
        }
    }
    spans
}

/// Every `aoe <words>` invocation in `content`'s code spans, as the words that
/// were read rather than the prefix of them that happened to resolve.
fn read_invocations(content: &str) -> Vec<Vec<String>> {
    let re = regex::Regex::new(r"\baoe[ \t]+([a-z][a-z0-9 \t-]*)").unwrap();
    let mut invocations = Vec::new();
    for span in code_spans(content) {
        for cap in re.captures_iter(span) {
            let words: Vec<String> = cap[1]
                .split_whitespace()
                // A leading `-` is a flag, not a subcommand: the capture runs
                // through `--json` and friends because `-` is a legal
                // character inside a command name too.
                .take_while(|w| {
                    !w.starts_with('-') && w.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                })
                .map(str::to_string)
                .collect();
            if !words.is_empty() {
                invocations.push(words);
            }
        }
    }
    invocations
}

/// Resolve one invocation against the CLI tree.
///
/// `Ok(Some(path))` is a canonical command to credit in the advisory,
/// `Ok(None)` an alias or a bare prefix with nothing to credit, and `Err` the
/// command text to report as nonexistent.
fn resolve_invocation(words: &[String], cli: &CliTree) -> Result<Option<String>, String> {
    let mut resolved: Option<(String, usize)> = None;
    let mut path = String::new();
    for (index, word) in words.iter().enumerate() {
        if path.is_empty() {
            path = word.clone();
        } else {
            path = format!("{path} {word}");
        }
        if cli.is_known(&path) {
            resolved = Some((path.clone(), index + 1));
        }
    }

    let Some((resolved, consumed)) = resolved else {
        return Err(words[0].clone());
    };
    // A word after a leaf command is a positional argument (`aoe group create
    // mygroup`); after a command that takes subcommands it is a subcommand
    // name, and an unknown one is the bug this check exists for.
    if cli.parents.contains(&resolved) {
        if let Some(next) = words.get(consumed) {
            return Err(format!("{resolved} {next}"));
        }
    }
    Ok(cli.commands.contains(&resolved).then_some(resolved))
}

/// Check every `aoe ...` invocation in one skill file, returning the canonical
/// commands to credit in the advisory and the nonexistent ones to report.
fn check_invocations(content: &str, cli: &CliTree) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut referenced = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for words in read_invocations(content) {
        match resolve_invocation(&words, cli) {
            Ok(Some(path)) => {
                referenced.insert(path);
            }
            Ok(None) => {}
            Err(bad) => {
                unknown.insert(bad);
            }
        }
    }
    (referenced, unknown)
}

/// How the skill's published version is sourced, which determines whether a
/// top-level `version:` field is allowed in the frontmatter.
enum VersionRule {
    /// clawhub manages the version via `_meta.json` and the release workflow's
    /// `--version` flag, so a static `version:` field would go stale: forbid it.
    Forbidden,
    /// The Hermes Skills Hub requires a top-level `version:` field: require it.
    Required,
}

fn check_skill() {
    let skills = [
        ("contrib/openclaw-skill/SKILL.md", VersionRule::Forbidden),
        ("contrib/hermes-skill/SKILL.md", VersionRule::Required),
    ];

    // Build the clap command tree once; shared across every skill file.
    let cli = CliTree::from_command(&agent_of_empires::cli::Cli::command());

    let mut has_error = false;
    let mut referenced: BTreeSet<String> = BTreeSet::new();

    for (path_str, version_rule) in &skills {
        let skill_path = Path::new(path_str);
        if !skill_path.exists() {
            eprintln!("Skill file not found: {}", skill_path.display());
            has_error = true;
            continue;
        }

        let content = fs::read_to_string(skill_path).expect("Failed to read SKILL.md");

        if check_skill_file(path_str, &content, version_rule, &cli, &mut referenced) {
            has_error = true;
        }
    }

    // Advisory: CLI commands not referenced in any skill file.
    let mut missing_from_skill = Vec::new();
    for cli_cmd in &cli.commands {
        let mentioned = referenced.iter().any(|s| {
            s == cli_cmd
                || cli_cmd.starts_with(&format!("{} ", s))
                || s.starts_with(&format!("{} ", cli_cmd))
        });
        if !mentioned {
            missing_from_skill.push(cli_cmd.clone());
        }
    }

    if !missing_from_skill.is_empty() {
        println!("Advisory: CLI commands not referenced in any skill file:");
        for cmd in &missing_from_skill {
            println!("  aoe {}", cmd);
        }
    }

    if has_error {
        std::process::exit(1);
    }

    println!("Skill check passed.");
}

/// Validate one skill file's frontmatter version rule and command references.
/// Referenced commands are accumulated into `referenced` for the shared
/// advisory. Returns `true` if an error was found.
fn check_skill_file(
    path_str: &str,
    content: &str,
    version_rule: &VersionRule,
    cli: &CliTree,
    referenced: &mut BTreeSet<String>,
) -> bool {
    let mut has_error = false;

    let has_version = content
        .strip_prefix("---\n")
        .and_then(|s| s.split_once("\n---"))
        .is_some_and(|(frontmatter, _)| {
            frontmatter.lines().any(|line| line.starts_with("version:"))
        });

    match version_rule {
        VersionRule::Forbidden if has_version => {
            eprintln!(
                "ERROR: {} frontmatter must not contain a top-level `version:` field; \
                 clawhub's _meta.json is the source of truth",
                path_str
            );
            has_error = true;
        }
        VersionRule::Required if !has_version => {
            eprintln!(
                "ERROR: {} frontmatter must contain a top-level `version:` field; \
                 the Hermes Skills Hub requires it",
                path_str
            );
            has_error = true;
        }
        _ => {}
    }

    let (found, unknown) = check_invocations(content, cli);
    for bad in unknown {
        eprintln!("ERROR: {path_str} references command 'aoe {bad}' which does not exist in CLI");
        has_error = true;
    }

    referenced.extend(found);
    has_error
}

#[cfg(test)]
mod skill_check_tests {
    use super::{check_invocations, code_spans, CliTree};
    use std::collections::BTreeSet;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn tree() -> CliTree {
        CliTree {
            commands: set(&[
                "list",
                "session",
                "session capture",
                "group",
                "group create",
            ]),
            parents: set(&["session", "group"]),
            aliases: set(&["ls", "group ls"]),
        }
    }

    #[test]
    fn code_spans_are_fences_and_paired_inline_spans() {
        let cases: &[(&str, &[&str])] = &[
            ("```sh\naoe list\n```", &["aoe list"]),
            ("Run `aoe list` now.", &["aoe list"]),
            (
                "Two `aoe list` and `aoe group` spans.",
                &["aoe list", "aoe group"],
            ),
            // Prose is not code: `aoe` is an ordinary word in these files.
            ("Use aoe list to see sessions.", &[]),
            // One backtick cannot delimit a span, so the line is skipped.
            ("A stray ` and aoe list after it.", &[]),
            // Line-local, so frontmatter cannot join two keys into a command.
            ("name: aoe\ndescription: something", &[]),
            // A `#` comment inside a fence is prose about a command, not an
            // invocation of one, so the tail is dropped.
            ("```sh\n# aoe list is the listing\n```", &[""]),
            ("```sh\naoe list # lists them\n```", &["aoe list "]),
            // Neither tilde fences nor indented blocks are code context here.
            ("~~~\naoe list\n~~~", &[]),
            ("    aoe list", &[]),
            // A non-shell fence is data. Reading it as commands turned a
            // sample payload or a prose block into a red check.
            ("```json\n\"note\": \"aoe manages sessions\"\n```", &[]),
            ("```text\naoe makes it easy to run agents\n```", &[]),
            // Its closing marker must not open a shell fence: that inverts
            // every fence after it, which silently drops real coverage.
            (
                "```json\n{}\n```\nprose aoe here\n```sh\naoe list\n```",
                &["aoe list"],
            ),
            ("```json\n{}\n```\nRun `aoe list`.", &["aoe list"]),
        ];
        for (content, expected) in cases {
            assert_eq!(&code_spans(content), expected, "spans of {content:?}");
        }
    }

    #[test]
    fn invocations_resolve_credit_and_reject() {
        // (content, credited commands, commands reported as nonexistent)
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("`aoe list`", &["list"], &[]),
            ("`aoe session capture`", &["session capture"], &[]),
            // #3479: none of these three could be reported before, and the
            // middle one also suppressed every `aoe session *` advisory entry.
            ("`aoe totallybogus`", &[], &["totallybogus"]),
            ("`aoe session bogusverb`", &[], &["session bogusverb"]),
            ("`aoe session pin`", &[], &["session pin"]),
            // A word after a leaf is a positional argument, not a subcommand.
            ("`aoe group create mygroup`", &["group create"], &[]),
            ("`aoe session capture my-id`", &["session capture"], &[]),
            // An alias is accepted as input but credits no canonical command.
            ("`aoe ls`", &[], &[]),
            ("`aoe group ls`", &[], &[]),
            // Flags and placeholders end the command path. A flag after a
            // command that takes subcommands is still a flag.
            ("`aoe list --json`", &["list"], &[]),
            ("`aoe group --help`", &["group"], &[]),
            ("`aoe session --json`", &["session"], &[]),
            ("`aoe session capture <id>`", &["session capture"], &[]),
            // A bare prefix is a real reference with nothing extra to check.
            ("`aoe session`", &["session"], &[]),
            // Repeats report once.
            (
                "`aoe totallybogus` and `aoe totallybogus`",
                &[],
                &["totallybogus"],
            ),
            // Prose is not scanned, so neither half of this is read.
            ("Use aoe totallybogus freely.", &[], &[]),
        ];
        let cli = tree();
        for (content, credited, unknown) in cases {
            let (referenced, reported) = check_invocations(content, &cli);
            assert_eq!(referenced, set(credited), "credited for {content:?}");
            assert_eq!(reported, set(unknown), "reported for {content:?}");
        }
    }

    /// A parent reached through an alias still has subcommands. Before this,
    /// `walk` recursed only under the canonical name, so `grp create` was in
    /// neither `commands` nor `aliases`, `grp` resolved as a parent, and the
    /// trailing word reported as a command that does not exist.
    #[test]
    fn aliases_expand_into_their_subcommand_paths() {
        let cmd = clap::Command::new("aoe").subcommand(
            clap::Command::new("group")
                .alias("grp")
                .subcommand(clap::Command::new("create").alias("new")),
        );
        let cli = CliTree::from_command(&cmd);
        for path in ["group", "grp", "group create", "grp create", "grp new"] {
            assert!(cli.is_known(path), "{path} must resolve");
        }
        assert_eq!(
            cli.commands,
            set(&["group", "group create"]),
            "only the fully canonical paths belong in the advisory"
        );
        // The parent rule still has to catch a bad subcommand under the alias.
        let (credited, unknown) = check_invocations("`aoe grp bogusverb`", &cli);
        assert!(credited.is_empty());
        assert_eq!(unknown, set(&["grp bogusverb"]));
        // And a real one through the alias must stay silent.
        let (credited, unknown) = check_invocations("`aoe grp create mygroup`", &cli);
        assert!(credited.is_empty() && unknown.is_empty());
    }

    #[test]
    fn aliases_are_read_from_the_clap_tree() {
        use clap::CommandFactory;
        let cli = CliTree::from_command(&agent_of_empires::cli::Cli::command());
        // `collect_subcommand_paths` recorded `get_name()` only, so every
        // `#[command(alias = ...)]` read as a nonexistent command (#3479).
        assert!(!cli.aliases.is_empty(), "the CLI declares command aliases");
        assert!(
            cli.aliases
                .iter()
                .all(|alias| !cli.commands.contains(alias)),
            "an alias must not also be advertised as a canonical command"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::is_watch_relevant;
    use std::path::Path;

    #[test]
    fn rust_sources_are_relevant() {
        assert!(is_watch_relevant(Path::new("src/main.rs")));
        assert!(is_watch_relevant(Path::new("src/server/mod.rs")));
        assert!(is_watch_relevant(Path::new(
            "/abs/agent-of-empires/src/tui/app.rs"
        )));
    }

    #[test]
    fn cargo_manifests_are_relevant() {
        assert!(is_watch_relevant(Path::new("Cargo.toml")));
        assert!(is_watch_relevant(Path::new("./Cargo.lock")));
        assert!(is_watch_relevant(Path::new("/abs/repo/Cargo.toml")));
    }

    #[test]
    fn unrelated_paths_are_ignored() {
        assert!(!is_watch_relevant(Path::new("README.md")));
        assert!(!is_watch_relevant(Path::new("target/debug/aoe")));
        assert!(!is_watch_relevant(Path::new(".git/index")));
        assert!(!is_watch_relevant(Path::new("Cargo.toml.swp")));
        assert!(!is_watch_relevant(Path::new("web/src/App.tsx")));
    }
}
