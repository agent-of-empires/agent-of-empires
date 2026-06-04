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

/// Build the serve-enabled debug binary. Returns whether the build succeeded so
/// the watch loop can keep the old backend running on a failed rebuild.
#[cfg(unix)]
fn build_serve() -> bool {
    use std::process::Command;
    eprintln!("[xtask dev] building aoe (--features serve)...");
    Command::new("cargo")
        .args(["build", "--features", "serve"])
        .status()
        .map(|s| s.success())
        .unwrap_or_else(|e| {
            eprintln!("[xtask dev] failed to run cargo build: {e}");
            false
        })
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

/// Build the serve-enabled binary, then run it alongside the Vite dev server.
/// Vite proxies `/api` and the `/sessions/*/ws` relays to the backend via the
/// `VITE_PROXY` env var it already honors. Each child runs in its own process
/// group so a single Ctrl-C tears the whole tree down (npm spawns vite, vite
/// may spawn esbuild) with no orphans.
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
    if !build_serve() {
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
    let spawn_serve = || -> Child {
        Command::new(&bin)
            .args(["serve", "--no-auth", "--port", &serve_port.to_string()])
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("failed to spawn `aoe serve`")
    };

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
                    if build_serve() {
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

fn collect_subcommand_paths(cmd: &clap::Command, prefix: &str, out: &mut BTreeSet<String>) {
    for sub in cmd.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let path = if prefix.is_empty() {
            sub.get_name().to_string()
        } else {
            format!("{} {}", prefix, sub.get_name())
        };
        out.insert(path.clone());
        collect_subcommand_paths(sub, &path, out);
    }
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
    let cli_cmd = agent_of_empires::cli::Cli::command();
    let mut cli_commands: BTreeSet<String> = BTreeSet::new();
    collect_subcommand_paths(&cli_cmd, "", &mut cli_commands);

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

        if check_skill_file(
            path_str,
            &content,
            version_rule,
            &cli_commands,
            &mut referenced,
        ) {
            has_error = true;
        }
    }

    // Advisory: CLI commands not referenced in any skill file.
    let mut missing_from_skill = Vec::new();
    for cli_cmd in &cli_commands {
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
    cli_commands: &BTreeSet<String>,
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

    // Extract `aoe <words>` patterns and match longest valid subcommand path
    let re = regex::Regex::new(r"aoe\s+([a-z][a-z0-9 -]*)").unwrap();
    let mut skill_commands: BTreeSet<String> = BTreeSet::new();
    for cap in re.captures_iter(content) {
        let raw = cap[1].trim();
        let words: Vec<&str> = raw
            .split_whitespace()
            .take_while(|w| {
                !w.starts_with('-')
                    && !w.starts_with('<')
                    && !w.starts_with('"')
                    && !w.starts_with('$')
                    && !w.starts_with('/')
                    && !w.starts_with('.')
                    && w.chars().all(|c| c.is_ascii_lowercase() || c == '-')
            })
            .collect();

        // Find the longest prefix that is a known CLI command
        let mut best = String::new();
        let mut path = String::new();
        for word in &words {
            if path.is_empty() {
                path = word.to_string();
            } else {
                path = format!("{} {}", path, word);
            }
            if cli_commands.contains(&path) {
                best = path.clone();
            }
        }
        // If no exact match, use the first word if it's a known top-level command
        if best.is_empty() && !words.is_empty() && cli_commands.contains(words[0]) {
            best = words[0].to_string();
        }
        if !best.is_empty() {
            skill_commands.insert(best);
        }
    }

    // Check for skill references to commands that don't exist
    for skill_cmd in &skill_commands {
        if !cli_commands.contains(skill_cmd) {
            let is_prefix = cli_commands
                .iter()
                .any(|c| c.starts_with(&format!("{} ", skill_cmd)));
            if !is_prefix {
                eprintln!(
                    "ERROR: {} references command 'aoe {}' which does not exist in CLI",
                    path_str, skill_cmd
                );
                has_error = true;
            }
        }
    }

    referenced.extend(skill_commands);
    has_error
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
