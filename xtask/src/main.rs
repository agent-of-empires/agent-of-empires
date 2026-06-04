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

/// Build the serve-enabled binary, then run it alongside the Vite dev server.
/// Vite proxies `/api` and the `/sessions/*/ws` relays to the backend via the
/// `VITE_PROXY` env var it already honors. Each child runs in its own process
/// group so a single Ctrl-C tears the whole tree down (npm spawns vite, vite
/// may spawn esbuild) with no orphans.
#[cfg(unix)]
fn run_dev(args: DevArgs) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // Build up front so build output doesn't interleave with Vite's startup
    // and a broken build fails fast before either server comes up.
    eprintln!("[xtask dev] building aoe (--features serve)...");
    let built = Command::new("cargo")
        .args(["build", "--features", "serve"])
        .status()
        .expect("failed to run cargo build");
    if !built.success() {
        std::process::exit(built.code().unwrap_or(1));
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
    let mut serve = Command::new(&bin)
        .args(["serve", "--no-auth", "--port", &args.serve_port.to_string()])
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("failed to spawn `aoe serve`");

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
            let _ = killpg(Pid::from_raw(serve.id() as i32), Signal::SIGTERM);
            let _ = serve.wait();
            std::process::exit(1);
        }
    };

    eprintln!(
        "[xtask dev] aoe serve on :{} | open http://localhost:{}",
        args.serve_port, args.web_port
    );

    let exited = |child: &mut Child| matches!(child.try_wait(), Ok(Some(_)));

    // Supervise: stop when Ctrl-C arrives or either child dies on its own.
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if exited(&mut serve) {
            eprintln!("[xtask dev] `aoe serve` exited; stopping vite");
            break;
        }
        if exited(&mut vite) {
            eprintln!("[xtask dev] vite exited; stopping `aoe serve`");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Signal each process group: SIGTERM, brief grace, then SIGKILL so the
    // ports are always freed even if a child ignores the term.
    let groups = [serve.id() as i32, vite.id() as i32];
    for pid in groups {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if exited(&mut serve) && exited(&mut vite) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !(exited(&mut serve) && exited(&mut vite)) {
        for pid in groups {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
    let _ = serve.wait();
    let _ = vite.wait();
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
