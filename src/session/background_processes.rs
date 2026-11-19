//! Which processes in an Agent's pane an update restart would stop that the Agent itself would
//! not bring back (#5762): the shells it started, named by the command they run.

use std::path::{Path, PathBuf};

use crate::process::{CommandSample, PrimeProcessIdentity};
use crate::session::Instance;

/// What a restart of `instance` would stop besides its Agent. Blocks on `ps` or `/proc`.
pub(crate) fn pane_census(instance: &Instance) -> PaneCensus {
    let Ok(session) = instance.tmux_session() else {
        return PaneCensus::Unclassified;
    };
    // A pane tmux can't classify might be running anything, so it isn't taken for an empty one.
    match crate::tmux::utils::probe_pane(session.name()) {
        crate::tmux::utils::PaneProbe::Missing => return PaneCensus::Background(Vec::new()),
        crate::tmux::utils::PaneProbe::Unknown => return PaneCensus::Unclassified,
        crate::tmux::utils::PaneProbe::Alive | crate::tmux::utils::PaneProbe::Dead => {}
    }
    let Some(tree) = session
        .get_pane_pid()
        .and_then(|pid| crate::process::process_tree_commands(pid).ok())
    else {
        return PaneCensus::Unclassified;
    };
    let launch = AgentLaunch::recorded(
        instance.launch_executable.as_deref(),
        instance.launch_resolved_executable.as_deref(),
    );
    match census(&tree, &launch, &|arg| canonical(Path::new(arg))) {
        PaneCensus::Background(found) if found.len() > MAX_LISTED => PaneCensus::Unclassified,
        census => census,
    }
}

/// The `background_processes` refusal's fields, or `None` when the restart may go ahead: every
/// process found was confirmed, or the census couldn't classify and the caller accepted that.
pub(crate) fn refusal(
    census: PaneCensus,
    confirmed: &[PrimeProcessIdentity],
    stop_unclassified: bool,
) -> Option<serde_json::Value> {
    match census {
        PaneCensus::Unclassified if stop_unclassified => None,
        PaneCensus::Unclassified => Some(serde_json::json!({
            "classified": false,
            "total": 0,
            "processes": [],
        })),
        PaneCensus::Background(found) => {
            if unacknowledged(found.clone(), confirmed).is_empty() {
                return None;
            }
            let processes: Vec<_> = found
                .iter()
                .enumerate()
                .map(|(index, process)| {
                    let mut entry = serde_json::json!({
                        "pid": process.identity.pid,
                        "start_id": process.identity.start_id,
                        "elapsed_secs": process.elapsed_secs,
                    });
                    if index < NAMED {
                        entry["argv"] = shown_argv(&process.argv).into();
                    }
                    entry
                })
                .collect();
            Some(serde_json::json!({
                "classified": true,
                "total": found.len(),
                "processes": processes,
            }))
        }
    }
}

/// A restart's confirmation, checked again just before its pane is stopped, after the launch
/// hooks ran without the locks.
pub(crate) struct ProcessRecheck {
    pub(crate) confirmed: Vec<PrimeProcessIdentity>,
    pub(crate) stop_unclassified: bool,
    pub(crate) admitted: AdmittedRestart,
}

/// The fields a restart's admission changes, as they were before and after it, so a late
/// refusal can put back those nobody has changed since. The admission takes a check only on a
/// restart that changes no setting.
#[derive(Debug, Clone)]
pub(crate) struct AdmittedRestart {
    before: Admitted,
    after: Admitted,
    last_error: Option<String>,
    last_error_check: Option<std::time::Instant>,
}

/// The stored fields an admission changes.
#[derive(Debug, Clone, PartialEq)]
struct Admitted {
    status: crate::session::Status,
    last_accessed_at: Option<chrono::DateTime<chrono::Utc>>,
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    idle_dormant_since: Option<chrono::DateTime<chrono::Utc>>,
    idle_entered_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Admitted {
    fn of(row: &Instance) -> Self {
        Self {
            status: row.status,
            last_accessed_at: row.last_accessed_at,
            archived_at: row.archived_at,
            snoozed_until: row.snoozed_until,
            idle_dormant_since: row.idle_dormant_since,
            idle_entered_at: row.idle_entered_at,
        }
    }
}

impl AdmittedRestart {
    /// Runs `admit` on `row`, recording the fields on either side of it.
    pub(crate) fn admit(
        row: &mut Instance,
        admit: impl FnOnce(&mut Instance) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let before = Admitted::of(row);
        let (last_error, last_error_check) = (row.last_error.clone(), row.last_error_check);
        admit(row)?;
        Ok(Self {
            before,
            after: Admitted::of(row),
            last_error,
            last_error_check,
        })
    }

    /// Puts back each stored field still holding what the admission set. A field changed since
    /// keeps its new value, unless the change set the very value the admission had, which can't
    /// be told apart without a per-edit revision. The error, never stored, is always put back.
    pub(crate) fn restore(&self, row: &mut Instance) {
        fn put_back<T: PartialEq + Clone>(field: &mut T, before: &T, after: &T) {
            if field == after {
                field.clone_from(before);
            }
        }
        let (before, after) = (&self.before, &self.after);
        put_back(&mut row.status, &before.status, &after.status);
        put_back(
            &mut row.last_accessed_at,
            &before.last_accessed_at,
            &after.last_accessed_at,
        );
        put_back(
            &mut row.archived_at,
            &before.archived_at,
            &after.archived_at,
        );
        put_back(
            &mut row.snoozed_until,
            &before.snoozed_until,
            &after.snoozed_until,
        );
        put_back(
            &mut row.idle_dormant_since,
            &before.idle_dormant_since,
            &after.idle_dormant_since,
        );
        put_back(
            &mut row.idle_entered_at,
            &before.idle_entered_at,
            &after.idle_entered_at,
        );
        row.last_error.clone_from(&self.last_error);
        row.last_error_check = self.last_error_check;
    }
}

/// The recheck found what the first check didn't: the refusal's fields, with the reservation
/// already released and the row restored.
#[derive(Debug)]
pub(crate) struct BackgroundProcessesAppeared(pub(crate) serde_json::Value);

impl std::fmt::Display for BackgroundProcessesAppeared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("background processes started after the restart's check")
    }
}

impl std::error::Error for BackgroundProcessesAppeared {}

/// How many processes a refusal names; the rest are counted.
const NAMED: usize = 5;
/// More background shells than this are reported as unclassified, so the refusal stays within
/// the client's 8 KiB error body.
const MAX_LISTED: usize = 24;
const ARG_CAP: usize = 256;
/// A shown command line's size as JSON.
const ARGV_CAP: usize = 768;
const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh"];

/// Where the Agent's own process can be recognised: the files its launch resolved to, and the
/// package directories they ship in (a launcher may exec a binary bundled beside it).
#[derive(Debug, Default)]
pub(crate) struct AgentLaunch {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
}

impl AgentLaunch {
    /// From the executable the launch recorded and where it resolved then.
    pub(crate) fn recorded(executable: Option<&Path>, resolved: Option<&Path>) -> Self {
        let mut launch = Self::default();
        for path in [
            resolved.map(Path::to_path_buf),
            executable.and_then(canonical),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(root) = package_root(&path) {
                if !launch.dirs.contains(&root) {
                    launch.dirs.push(root);
                }
            }
            if !launch.files.contains(&path) {
                launch.files.push(path);
            }
        }
        launch
    }

    fn matches(&self, path: &Path) -> bool {
        self.files.iter().any(|file| file == path)
            || self.dirs.iter().any(|dir| path.starts_with(dir))
    }
}

fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// The nearest directory above `file` holding a `package.json`, within a few levels.
fn package_root(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .take(4)
        .find(|dir| dir.join("package.json").is_file())
        .map(Path::to_path_buf)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundProcess {
    pub identity: PrimeProcessIdentity,
    pub argv: Vec<String>,
    pub elapsed_secs: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaneCensus {
    /// No single Agent process was found, so nothing can be named.
    Unclassified,
    Background(Vec<BackgroundProcess>),
}

/// The background shells in `tree` (root first, as `process_tree_commands` returns it).
/// `resolve` canonicalizes a path an argument names.
pub(crate) fn census(
    tree: &[CommandSample],
    launch: &AgentLaunch,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> PaneCensus {
    let matches = |sample: &CommandSample| {
        // The program, or the script an interpreter runs: its first argument past any flags. A
        // shell given `-c` runs a command, not a script, so `bash -lc <agent>` isn't the Agent.
        let mut args = sample.argv.iter().skip(1);
        let script = args.find(|arg| !arg.starts_with('-') || runs_command(sample, arg));
        let script = script.filter(|arg| !arg.starts_with('-'));
        sample.exe.as_deref().is_some_and(|exe| launch.matches(exe))
            || sample.argv.first().into_iter().chain(script).any(|arg| {
                arg.contains('/') && resolve(arg).is_some_and(|path| launch.matches(&path))
            })
    };
    let parent_of = |pid: u32| {
        tree.iter()
            .find(|s| s.identity.pid == pid)
            .map(|s| s.parent)
    };
    let is_ancestor = |ancestor: u32, mut pid: u32| loop {
        if pid == ancestor {
            break true;
        }
        match parent_of(pid) {
            Some(parent) if parent != pid => pid = parent,
            _ => break false,
        }
    };
    let matched: Vec<_> = tree.iter().filter(|sample| matches(sample)).collect();
    let Some(mut agent) = matched.iter().copied().find(|outer| {
        matched
            .iter()
            .all(|other| is_ancestor(outer.identity.pid, other.identity.pid))
    }) else {
        return PaneCensus::Unclassified;
    };
    // A launcher that relaunches itself as its only child: the Agent is the child. A match
    // beside other children is a helper the Agent bundles, or a copy one of its shells runs.
    loop {
        let mut children = tree.iter().filter(|s| s.parent == agent.identity.pid);
        match (children.next(), children.next()) {
            (Some(only), None) if matches(only) => agent = only,
            _ => break,
        }
    }
    PaneCensus::Background(
        tree.iter()
            .filter(|sample| sample.parent == agent.identity.pid && is_shell(sample))
            .map(|shell| BackgroundProcess {
                identity: shell.identity.clone(),
                argv: named_command(tree, shell),
                elapsed_secs: shell.elapsed_secs,
            })
            .collect(),
    )
}

/// Whether `flag` is a shell's `-c` (alone or among other short flags, as in `-lc`), or fish's
/// `--command`, with its script after it or attached (`--command=…`).
fn runs_command(sample: &CommandSample, flag: &str) -> bool {
    is_shell(sample)
        && ((flag.starts_with('-') && !flag.starts_with("--") && flag.contains('c'))
            || flag == "--command"
            || flag.starts_with("--command="))
}

fn is_shell(sample: &CommandSample) -> bool {
    let name = |path: &str| {
        let base = path.rsplit('/').next().unwrap_or(path);
        SHELLS.contains(&base.trim_start_matches('-'))
    };
    // busybox's `sh` runs as `busybox`, so argv[0] counts too.
    sample
        .exe
        .as_deref()
        .and_then(Path::to_str)
        .is_some_and(name)
        || sample.argv.first().is_some_and(|arg0| name(arg0))
}

/// What `shell` runs: its first descendant that isn't another shell, or else the program the
/// shell was given. A `-c` script is never shown: it is one argument, so a secret inside it
/// can't be blanked.
fn named_command(tree: &[CommandSample], shell: &CommandSample) -> Vec<String> {
    let mut level = vec![shell.identity.pid];
    while !level.is_empty() {
        let children: Vec<_> = tree.iter().filter(|s| level.contains(&s.parent)).collect();
        if let Some(command) = children.iter().find(|child| !is_shell(child)) {
            return command.argv.clone();
        }
        level = children.iter().map(|child| child.identity.pid).collect();
    }
    wrapped_command(&shell.argv).unwrap_or_else(|| {
        let script_flag = shell
            .argv
            .iter()
            .skip(1)
            .position(|arg| runs_command(shell, arg));
        match script_flag {
            Some(at) => {
                let mut shown = shell.argv[..at + 1].to_vec();
                shown.push(
                    shell.argv[at + 1]
                        .split_once('=')
                        .map_or(shell.argv[at + 1].as_str(), |(flag, _)| flag)
                        .to_owned(),
                );
                shown
            }
            None => shell.argv.clone(),
        }
    })
}

/// The program inside Claude Code's Bash wrapper, `bash -c "source <snapshot> ... && eval
/// '<command>' < /dev/null ..."`: its first word past any `NAME=value` and `env` prefix, and
/// only when that word reads exactly as written, since nothing after it is shown.
fn wrapped_command(argv: &[String]) -> Option<Vec<String>> {
    if !argv.iter().any(|arg| arg == "-c") {
        return None;
    }
    let script = argv.last()?.strip_prefix("source ")?;
    let (_, quoted) = script.split_once("&& eval ")?;
    let command = shell_word(quoted)?;
    let mut after_env = false;
    for word in command.split_whitespace() {
        if word.contains(['\'', '"', '`', '$', '\\', ';', '&', '|', '<', '>', '(', ')']) {
            return None;
        }
        let assignment = word.split_once('=').is_some_and(|(name, _)| {
            name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if assignment {
            continue;
        }
        if word == "env" && !after_env {
            after_env = true;
            continue;
        }
        if after_env && word.starts_with('-') {
            return None;
        }
        return Some(vec![word.to_owned()]);
    }
    None
}

/// The first shell word of `text`: adjacent single- and double-quoted parts joined, as the shell
/// reads them. `None` for an unterminated quote.
fn shell_word(text: &str) -> Option<String> {
    let mut word = String::new();
    let mut chars = text.chars();
    loop {
        match chars.next() {
            None => return Some(word),
            Some(c) if c.is_whitespace() => return Some(word),
            Some('\'') => loop {
                match chars.next()? {
                    '\'' => break,
                    c => word.push(c),
                }
            },
            Some('"') => loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => word.push(chars.next()?),
                    c => word.push(c),
                }
            },
            Some('\\') => word.push(chars.next()?),
            Some(c) => word.push(c),
        }
    }
}

/// The found processes the caller hasn't agreed to stop. A reused PID has a new start identity,
/// so it counts as new.
pub(crate) fn unacknowledged(
    found: Vec<BackgroundProcess>,
    acknowledged: &[PrimeProcessIdentity],
) -> Vec<BackgroundProcess> {
    found
        .into_iter()
        .filter(|process| !acknowledged.contains(&process.identity))
        .collect()
}

/// `argv` as it may leave the host: credentials blanked (see `redacted`), control characters
/// replaced, each argument and the whole line capped.
pub(crate) fn shown_argv(argv: &[String]) -> Vec<String> {
    let mut shown = Vec::new();
    let mut size = 2;
    let mut hide = Hide::Nothing;
    for arg in argv {
        let arg: String = redacted(arg, &mut hide)
            .chars()
            .map(|c| if c.is_control() { '\u{fffd}' } else { c })
            .collect();
        let arg = capped(&arg, ARG_CAP);
        size += serde_json::to_string(&arg).map_or(usize::MAX, |json| json.len()) + 1;
        if size > ARGV_CAP {
            break;
        }
        shown.push(arg);
    }
    shown
}

const REDACTED: &str = "***";

/// What a flag hides of the argument after it.
#[derive(Clone, Copy)]
enum Hide {
    Nothing,
    /// The whole value of a secret flag (`--token abc`).
    Value,
    /// The password of a `user:password` value (`curl -u`).
    Password,
}

/// `arg` with its credentials blanked: the value after a secret flag, of a secret variable or
/// header (`Authorization: Bearer …`), and a URL's or `--user`'s password. `hide` carries a
/// secret flag over to the argument after it.
fn redacted(arg: &str, hide: &mut Hide) -> String {
    match std::mem::replace(hide, Hide::Nothing) {
        Hide::Value => return REDACTED.to_owned(),
        Hide::Password => {
            if let Some((user, _)) = arg.split_once(':') {
                return format!("{user}:{REDACTED}");
            }
        }
        Hide::Nothing => {}
    }
    if let Some((name, _)) = arg
        .split_once(':')
        .filter(|(name, _)| !name.contains(char::is_whitespace) && secret_name(name))
    {
        return format!("{name}: {REDACTED}");
    }
    // Before the word split below, so a value with spaces (`--token=alpha beta`) is hidden whole.
    if let Some((name, _)) = arg
        .split_once('=')
        .filter(|(name, _)| !name.contains(char::is_whitespace) && secret_name(name))
    {
        return format!("{name}={REDACTED}");
    }
    if arg.contains(char::is_whitespace) {
        // A process that rewrites its title can report several words as one argument.
        let mut words = Vec::new();
        for word in arg.split_whitespace() {
            if word.strip_suffix(':').is_some_and(secret_name) {
                words.extend([word.to_owned(), REDACTED.to_owned()]);
                break;
            }
            words.push(redacted(word, hide));
        }
        return words.join(" ");
    }
    let arg = without_url_password(arg);
    if let Some((name, _)) = arg.split_once('=').filter(|(name, _)| secret_name(name)) {
        return format!("{name}={REDACTED}");
    }
    // A `user:password` value attached to its flag (`--user=admin:pw`, `-uadmin:pw`).
    if let Some((flag, user)) = ["--user=", "--proxy-user=", "-u"].iter().find_map(|flag| {
        let (user, _) = arg.strip_prefix(flag)?.split_once(':')?;
        Some((flag, user))
    }) {
        return format!("{flag}{user}:{REDACTED}");
    }
    if arg.starts_with('-') && secret_name(&arg) {
        *hide = Hide::Value;
    } else if matches!(arg.as_str(), "-u" | "--user" | "--proxy-user") {
        *hide = Hide::Password;
    }
    arg
}

/// `arg` with the password of any URL in it (`scheme://user:password@host`) blanked. A `@` later
/// in the argument can only blank more than the password, never less.
fn without_url_password(arg: &str) -> String {
    let mut shown = String::new();
    let mut rest = arg;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        shown.push_str(head);
        rest = tail;
        if let Some((user, info)) = tail
            .rsplit_once('@')
            .and_then(|(info, _)| Some((info.split_once(':')?.0, info)))
        {
            shown.push_str(&format!("{user}:{REDACTED}"));
            rest = &tail[info.len()..];
        }
    }
    shown.push_str(rest);
    shown
}

/// A flag (`--api-key`), variable (`GITHUB_TOKEN`) or header (`Authorization`) whose value is a
/// credential.
fn secret_name(name: &str) -> bool {
    let name = name
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('-', "_");
    const EXACT: &[&str] = &[
        "token",
        "password",
        "passwd",
        "pass",
        "passphrase",
        "secret",
        "auth",
        "authorization",
        "apikey",
        "key",
        "bearer",
        "credential",
        "credentials",
        "cookie",
    ];
    const SUFFIXES: &[&str] = &[
        "_token",
        "_key",
        "_secret",
        "_password",
        "_passwd",
        "_passphrase",
        "_auth",
        "_authorization",
        "_credential",
        "_credentials",
        "_cookie",
    ];
    EXACT.contains(&name.as_str()) || SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

fn capped(arg: &str, cap: usize) -> String {
    if arg.len() <= cap {
        return arg.to_owned();
    }
    let mut end = cap.saturating_sub('…'.len_utf8());
    while !arg.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &arg[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, parent: u32, exe: &str, argv: &[&str]) -> CommandSample {
        CommandSample {
            identity: PrimeProcessIdentity {
                pid,
                start_id: format!("proc:{pid}0"),
            },
            parent,
            exe: Some(exe.into()),
            argv: argv.iter().map(|arg| arg.to_string()).collect(),
            elapsed_secs: u64::from(pid),
        }
    }

    fn named(census: PaneCensus) -> Option<Vec<Vec<String>>> {
        match census {
            PaneCensus::Unclassified => None,
            PaneCensus::Background(found) => Some(found.into_iter().map(|p| p.argv).collect()),
        }
    }

    #[test]
    fn only_the_shells_the_agent_started_are_named_by_what_they_run() {
        let claude = AgentLaunch {
            files: vec!["/opt/claude/versions/2.1.0".into()],
            dirs: vec![],
        };
        let npm_claude = AgentLaunch {
            files: vec!["/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js".into()],
            dirs: vec!["/usr/lib/node_modules/@anthropic-ai/claude-code".into()],
        };
        // From capture/fixtures: codex's launcher execs its vendor binary under the package.
        let codex = AgentLaunch {
            files: vec!["/usr/lib/node_modules/@openai/codex/bin/codex.js".into()],
            dirs: vec!["/usr/lib/node_modules/@openai/codex".into()],
        };
        let gemini = AgentLaunch {
            files: vec!["/usr/lib/node_modules/@google/gemini-cli/bundle/gemini.js".into()],
            dirs: vec!["/usr/lib/node_modules/@google/gemini-cli".into()],
        };
        let login = sample(10, 1, "/usr/bin/zsh", &["-zsh", "-lc", "claude"]);
        let dev_shell = |parent| {
            sample(
                30,
                parent,
                "/usr/bin/bash",
                &["bash", "-c", "-l", "source /tmp/snap && eval 'npm run dev'"],
            )
        };
        let npm = sample(31, 30, "/usr/bin/node", &["npm", "run", "dev"]);
        let esbuild = sample(32, 31, "/opt/esbuild", &["esbuild", "--watch"]);
        let resolve = |arg: &str| -> Option<PathBuf> {
            Some(match arg {
                "/usr/bin/gemini" | "/home/u/.npm/bin/gemini" => {
                    "/usr/lib/node_modules/@google/gemini-cli/bundle/gemini.js".into()
                }
                other => other.into(),
            })
        };
        let cases: Vec<(&str, &AgentLaunch, Vec<CommandSample>, Option<Vec<&[&str]>>)> = vec![
            (
                "login shell, native claude, a dev server: one entry",
                &claude,
                vec![
                    login.clone(),
                    sample(20, 10, "/opt/claude/versions/2.1.0", &["claude"]),
                    dev_shell(20),
                    npm.clone(),
                    esbuild.clone(),
                ],
                Some(vec![&["npm", "run", "dev"]]),
            ),
            (
                "an updated binary still runs from its recorded path",
                &claude,
                vec![
                    sample(20, 1, "/opt/claude/versions/2.1.0", &["claude"]),
                    dev_shell(20),
                ],
                Some(vec![&["npm"]]),
            ),
            (
                "a login shell whose -c names the Agent is not the Agent",
                &claude,
                vec![
                    sample(
                        10,
                        1,
                        "/usr/bin/bash",
                        &["bash", "-lc", "/opt/claude/versions/2.1.0"],
                    ),
                    sample(20, 10, "/opt/claude/versions/2.1.0", &["claude"]),
                    sample(21, 10, "/usr/bin/cat", &["cat"]),
                    dev_shell(20),
                    npm.clone(),
                ],
                Some(vec![&["npm", "run", "dev"]]),
            ),
            (
                // macOS reads arguments whole or not at all; without them nothing is named.
                "an npm Agent whose arguments couldn't be read: unclassified",
                &npm_claude,
                vec![
                    login.clone(),
                    sample(20, 10, "/usr/bin/node", &[]),
                    dev_shell(20),
                    npm.clone(),
                ],
                None,
            ),
            (
                "npm claude with an MCP server: nothing",
                &npm_claude,
                vec![
                    login.clone(),
                    sample(
                        20,
                        10,
                        "/usr/bin/node",
                        &[
                            "node",
                            "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                        ],
                    ),
                    sample(21, 20, "/usr/bin/node", &["npx", "some-mcp"]),
                    sample(22, 21, "/usr/bin/sh", &["sh", "-c", "some-mcp"]),
                ],
                Some(vec![]),
            ),
            (
                "codex: the vendor binary, its git child excluded, a shell named",
                &codex,
                vec![
                    sample(
                        20,
                        1,
                        "/usr/lib/node_modules/@openai/codex/vendor/x/bin/codex",
                        &["/usr/lib/node_modules/@openai/codex/vendor/x/bin/codex"],
                    ),
                    sample(21, 20, "/usr/bin/git", &["git", "fetch"]),
                    sample(22, 20, "/usr/bin/bash", &["bash", "-lc", "sleep 600"]),
                    sample(23, 22, "/usr/bin/sleep", &["sleep", "600"]),
                ],
                Some(vec![&["sleep", "600"]]),
            ),
            (
                "gemini relaunches itself: its shells hang off the child",
                &gemini,
                vec![
                    sample(20, 1, "/usr/bin/node", &["node", "/usr/bin/gemini"]),
                    sample(
                        21,
                        20,
                        "/usr/bin/node",
                        &[
                            "/usr/bin/node",
                            "--max-old-space-size=7978",
                            "/home/u/.npm/bin/gemini",
                        ],
                    ),
                    sample(22, 21, "/usr/bin/bash", &["bash", "-c", "tail -f log"]),
                    sample(23, 22, "/usr/bin/tail", &["tail", "-f", "log"]),
                ],
                Some(vec![&["tail", "-f", "log"]]),
            ),
            (
                "two background shells",
                &claude,
                vec![
                    sample(20, 1, "/opt/claude/versions/2.1.0", &["claude"]),
                    dev_shell(20),
                    npm.clone(),
                    sample(
                        40,
                        20,
                        "/usr/bin/bash",
                        &["bash", "-c", "while :; do :; done"],
                    ),
                ],
                Some(vec![&["npm", "run", "dev"], &["bash", "-c"]]),
            ),
            (
                "a bundled helper and a nested copy don't move the Agent",
                &codex,
                vec![
                    sample(
                        20,
                        1,
                        "/usr/lib/node_modules/@openai/codex/vendor/x/bin/codex",
                        &["codex"],
                    ),
                    sample(
                        21,
                        20,
                        "/usr/lib/node_modules/@openai/codex/vendor/x/rg",
                        &["rg"],
                    ),
                    sample(22, 20, "/usr/bin/bash", &["bash", "-c", "codex exec x"]),
                    sample(
                        23,
                        22,
                        "/usr/lib/node_modules/@openai/codex/vendor/x/bin/codex",
                        &["codex", "exec", "x"],
                    ),
                    sample(24, 20, "/bin/busybox", &["sh", "-c", "sleep 9"]),
                ],
                Some(vec![&["codex", "exec", "x"], &["sh", "-c"]]),
            ),
            (
                "no Agent process in the pane",
                &claude,
                vec![login.clone(), dev_shell(10), npm.clone()],
                None,
            ),
            (
                "two unrelated matches",
                &claude,
                vec![
                    login.clone(),
                    sample(20, 10, "/opt/claude/versions/2.1.0", &["claude"]),
                    sample(21, 10, "/opt/claude/versions/2.1.0", &["claude"]),
                ],
                None,
            ),
        ];
        for (name, launch, tree, expected) in cases {
            let expected = expected.map(|entries| {
                entries
                    .into_iter()
                    .map(|argv| argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            });
            assert_eq!(named(census(&tree, launch, &resolve)), expected, "{name}");
        }
    }

    #[test]
    fn a_wrapped_tool_command_is_named_by_what_it_evaluates() {
        // Claude Code 2.1's Bash tool, captured from this machine's own `ps`.
        let captured = "source /root/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null \
            || true && shopt -u extglob 2>/dev/null || true && { \\builtin unalias -- 'which'; \
            } >/dev/null 2>&1 || true && eval 'grep -c '\"'\"'a b'\"'\"' log' < /dev/null \
            && pwd -P >| /tmp/claude-4d4e-cwd";
        for (argv, named) in [
            (vec!["/bin/bash", "-c", captured], Some(vec!["grep"])),
            (
                vec![
                    "bash",
                    "-c",
                    "-l",
                    "source /tmp/snap && eval 'npm run dev' < /dev/null",
                ],
                Some(vec!["npm"]),
            ),
            (vec!["bash", "-c", "sleep 600"], None),
            (vec!["bash", "script.sh"], None),
            (
                vec!["bash", "-c", "source /tmp/snap && eval 'unterminated"],
                None,
            ),
            (vec!["bash", "-c", "source /tmp/snap && eval ''"], None),
            // The first word can carry a secret itself, so assignments go and an inexact word
            // isn't shown.
            (
                vec!["bash", "-c", "source /s && eval 'TOKEN=abc cli --token x'"],
                Some(vec!["cli"]),
            ),
            (
                vec!["bash", "-c", "source /s && eval 'env API_KEY=x A=1 cli'"],
                Some(vec!["cli"]),
            ),
            (vec!["bash", "-c", "source /s && eval 'env -i cli'"], None),
            (
                vec!["bash", "-c", "source /s && eval 'TOKEN=\"a b\" cli'"],
                None,
            ),
            (vec!["bash", "-c", "source /s && eval '$(cat key) x'"], None),
            (vec!["bash", "-c", "source /s && eval 'a\\ b'"], None),
            (vec!["bash", "-c", "source /s && eval '\"cli\" x'"], None),
        ] {
            let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
            let named = named.map(|words| words.iter().map(|w| w.to_string()).collect::<Vec<_>>());
            assert_eq!(wrapped_command(&argv), named, "{argv:?}");
        }
        let shell = sample(
            30,
            20,
            "/usr/bin/bash",
            &["bash", "-c", "-l", "source /s && eval 'make watch'"],
        );
        assert_eq!(
            named_command(std::slice::from_ref(&shell), &shell),
            ["make"]
        );
        // A shell left at a builtin shows no script, however it was given.
        for (argv, shown) in [
            (
                &["bash", "-c", "cli --token abc; read -r"][..],
                &["bash", "-c"][..],
            ),
            (
                &["bash", "-lc", "cli --token abc; read -r"],
                &["bash", "-lc"],
            ),
            (
                &["bash", "--norc", "-c", "x", "arg0", "--token", "y"],
                &["bash", "--norc", "-c"],
            ),
            (
                &["bash", "script.sh", "--token", "y"],
                &["bash", "script.sh", "--token", "y"],
            ),
            (
                &["fish", "--command", "cli --token abc; read"],
                &["fish", "--command"],
            ),
            (
                &["fish", "--command=cli --token abc; read"],
                &["fish", "--command"],
            ),
            (
                &["fish", "--no-config", "--command=cli --token abc"],
                &["fish", "--no-config", "--command"],
            ),
        ] {
            let plain = sample(31, 20, &format!("/usr/bin/{}", argv[0]), argv);
            assert_eq!(
                named_command(std::slice::from_ref(&plain), &plain),
                shown,
                "{argv:?}"
            );
        }
    }

    #[test]
    fn no_part_of_a_spaced_secret_leaves_the_host() {
        let eval = r#"source /s && eval 'cli --token "alpha beta"' < /dev/null"#;
        let exact = sample(31, 30, "/usr/bin/cli", &["cli", "--password", "alpha beta"]);
        for (tree, shown) in [
            (
                vec![sample(30, 20, "/usr/bin/bash", &["bash", "-c", eval])],
                &["cli"][..],
            ),
            (
                vec![sample(
                    30,
                    20,
                    "/usr/bin/bash",
                    &["bash", "-c", r#"cli --password "alpha beta""#],
                )],
                &["bash", "-c"],
            ),
            (
                vec![sample(
                    30,
                    20,
                    "/usr/bin/bash",
                    &["bash", "-lc", r#"cli --token "alpha beta"; read -r"#],
                )],
                &["bash", "-lc"],
            ),
            // Arguments read whole keep the value as one, so it is blanked whole.
            (
                vec![sample(30, 20, "/usr/bin/bash", &["bash", "-c", "x"]), exact],
                &["cli", "--password", "***"],
            ),
        ] {
            let named = shown_argv(&named_command(&tree, &tree[0]));
            assert!(
                !named
                    .iter()
                    .any(|arg| arg.contains("alpha") || arg.contains("beta")),
                "{named:?}"
            );
            assert_eq!(named, shown);
        }
    }

    #[test]
    fn a_confirmed_process_is_matched_by_its_start_not_its_pid() {
        let found = |pid, start: &str| BackgroundProcess {
            identity: PrimeProcessIdentity {
                pid,
                start_id: start.into(),
            },
            argv: vec![],
            elapsed_secs: 0,
        };
        let confirmed = [PrimeProcessIdentity {
            pid: 30,
            start_id: "proc:1".into(),
        }];
        let left = unacknowledged(
            vec![
                found(30, "proc:1"),
                found(30, "proc:2"),
                found(31, "proc:1"),
            ],
            &confirmed,
        );
        let left: Vec<_> = left
            .iter()
            .map(|p| (p.identity.pid, p.identity.start_id.as_str()))
            .collect();
        assert_eq!(left, [(30, "proc:2"), (31, "proc:1")]);
    }

    #[test]
    fn secret_values_are_blanked_before_a_command_line_leaves_the_host() {
        for (argv, shown) in [
            (&["npm", "run", "dev"][..], &["npm", "run", "dev"][..]),
            (
                &["cli", "--token", "abc", "next"],
                &["cli", "--token", "***", "next"],
            ),
            (&["cli", "--token=abc"], &["cli", "--token=***"]),
            (&["cli", "--token=alpha beta"], &["cli", "--token=***"]),
            (
                &["env", "API_KEY=alpha beta", "node"],
                &["env", "API_KEY=***", "node"],
            ),
            (&["cli", "--password", "pw"], &["cli", "--password", "***"]),
            (
                &["cli", "--api-key", "k", "--verbose"],
                &["cli", "--api-key", "***", "--verbose"],
            ),
            (
                &["cli", "--client-secret=s"],
                &["cli", "--client-secret=***"],
            ),
            (
                &["env", "OPENAI_API_KEY=sk-1", "node"],
                &["env", "OPENAI_API_KEY=***", "node"],
            ),
            (
                &["env", "GITHUB_TOKEN=x", "PASSWORD=y"],
                &["env", "GITHUB_TOKEN=***", "PASSWORD=***"],
            ),
            (
                &["gpg", "--passphrase", "p"],
                &["gpg", "--passphrase", "***"],
            ),
            (&["cli", "--key", "k"], &["cli", "--key", "***"]),
            (&["cli", "--bearer=b"], &["cli", "--bearer=***"]),
            // A header, a `--user` password and a URL's password.
            (
                &["curl", "-H", "Authorization: Bearer abc", "-u", "admin:pw"],
                &["curl", "-H", "Authorization: ***", "-u", "admin:***"],
            ),
            (
                &["curl", "--user=admin:pw", "-uadmin:pw", "--proxy-user=p:x"],
                &[
                    "curl",
                    "--user=admin:***",
                    "-uadmin:***",
                    "--proxy-user=p:***",
                ],
            ),
            (
                &["curl", "-H", "X-Api-Key:k"],
                &["curl", "-H", "X-Api-Key: ***"],
            ),
            (
                &[
                    "psql",
                    "postgres://user:pw@db/app",
                    "DATABASE_URL=mysql://u:p@h",
                ],
                &[
                    "psql",
                    "postgres://user:***@db/app",
                    "DATABASE_URL=mysql://u:***@h",
                ],
            ),
            // A process that rewrote its title reports its words as one argument.
            (
                &["node server.js --token abc -H Authorization: Bearer x"],
                &["node server.js --token *** -H Authorization: ***"],
            ),
            // Not secrets: a key-like word that isn't a flag, a path, `-p` (a port or a prompt), a
            // URL without a password, a `-u` that takes no pair.
            (
                &["cli", "token", "keys.json"],
                &["cli", "token", "keys.json"],
            ),
            (
                &["curl", "https://example.com/a@b", "localhost:3000"],
                &["curl", "https://example.com/a@b", "localhost:3000"],
            ),
            (&["python", "-u", "app.py"], &["python", "-u", "app.py"]),
            (
                &["ssh", "-p", "22", "--keyfile=/k"],
                &["ssh", "-p", "22", "--keyfile=/k"],
            ),
            (&["vite", "--port=5173"], &["vite", "--port=5173"]),
        ] {
            let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
            assert_eq!(shown_argv(&argv), shown, "{argv:?}");
        }
    }

    #[test]
    fn the_widest_refusal_fits_the_clients_error_body() {
        let quotes = "\"".repeat(400);
        let control = "\u{1b}[2J".repeat(100);
        let shown = shown_argv(&[control, quotes.clone(), "x".into(), quotes]);
        assert!(shown.iter().all(|arg| arg.len() <= ARG_CAP));
        assert!(!shown.iter().any(|arg| arg.contains('\u{1b}')), "{shown:?}");
        assert!(serde_json::to_string(&shown).unwrap().len() <= ARGV_CAP);

        let widest = BackgroundProcess {
            identity: PrimeProcessIdentity {
                pid: u32::MAX,
                start_id: format!("audit:{}", "f".repeat(64)),
            },
            argv: vec!["\"".into(); 4096],
            elapsed_secs: u64::MAX,
        };
        let fields = refusal(PaneCensus::Background(vec![widest; MAX_LISTED]), &[], false).unwrap();
        let mut body = serde_json::json!({
            "error": "background_processes",
            "message": "x".repeat(200),
        });
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let size = serde_json::to_string(&body).unwrap().len();
        assert!(size <= 8 * 1024, "{size}");
    }

    #[test]
    fn a_restart_goes_ahead_once_everything_found_is_confirmed() {
        let found = |pid| BackgroundProcess {
            identity: PrimeProcessIdentity {
                pid,
                start_id: format!("proc:{pid}"),
            },
            argv: vec!["sleep".into(), pid.to_string()],
            elapsed_secs: 7,
        };
        let all: Vec<_> = (1..=7).map(found).collect();
        let ids: Vec<_> = all.iter().map(|p| p.identity.clone()).collect();
        let listed = refusal(PaneCensus::Background(all.clone()), &ids[..6], false).unwrap();
        assert_eq!(listed["total"], 7);
        assert_eq!(listed["processes"].as_array().unwrap().len(), 7);
        assert_eq!(
            listed["processes"][4]["argv"],
            serde_json::json!(["sleep", "5"])
        );
        assert!(
            listed["processes"][5].get("argv").is_none(),
            "only the first 5 are named"
        );
        assert_eq!(listed["processes"][6]["start_id"], "proc:7");
        for (census, confirmed, unclassified, refused) in [
            (PaneCensus::Background(all.clone()), &ids[..], false, false),
            (PaneCensus::Background(vec![]), &[][..], false, false),
            (PaneCensus::Unclassified, &ids[..], false, true),
            (PaneCensus::Unclassified, &[][..], true, false),
        ] {
            assert_eq!(refusal(census, confirmed, unclassified).is_some(), refused);
        }
    }
}
