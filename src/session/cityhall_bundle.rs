//! The CityHall config bundle: one document describing how a CityHall-hosted
//! aoe workspace should be set up (#8).
//!
//! CityHall runs one locked-down `aoe serve --cityhall` per user, and in that
//! mode the routes that would configure it are closed: `PATCH /api/settings`,
//! `POST /api/projects` and `POST /api/git/clone` all sit in
//! `CITYHALL_MUTATION_DENY`. So configuration cannot arrive over the workspace's
//! own API; it arrives as this document, applied at boot below the HTTP layer.
//!
//! The round trip is:
//!
//! 1. An admin configures a normal aoe install, then [`export`]s a bundle
//!    (`aoe cityhall export`, or the Settings page's CityHall tab).
//! 2. CityHall stores that file and serves it, per user, to each workspace it
//!    spawns, splicing in the `[git]` section (see below).
//! 3. The workspace fetches it at boot and [`apply`]s it.
//!
//! Two deliberate shapes:
//!
//! - `settings` is a **sparse** patch keyed by section then field, the same
//!   shape a `PATCH /api/settings` body has, so it validates through
//!   [`validate_patch`] and merges through [`merge_json`] with no second code
//!   path. Only leaves that differ from [`Config::default`] are exported.
//! - `projects` carries **remotes, not paths**. A [`Project`] is a path to an
//!   already-cloned repo, and the admin's `/Users/me/src/foo` means nothing
//!   inside a container, so [`apply`] clones each remote into the workspace and
//!   registers the resulting path.
//!
//! `[git]` is never exported and never stored by CityHall: it holds a
//! credential, and CityHall composes it per user when it serves the document.
//!
//! Known limit: a bundle sets values, it cannot unset them. A field whose value
//! is `null` (a cleared `Option`) is dropped on export, because TOML has no way
//! to spell it. Setting a field to some other value works normally.

use anyhow::{anyhow, bail, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::config::settings_schema::{
    apply_changed_leaves, merge_json, schema, strip_local_only, validate_patch, Scope,
};
use super::config::{update_config, Config};
use super::get_app_dir;
use super::projects::{self, Project, ProjectScope};

/// Format version. [`apply`] refuses anything else rather than guessing at a
/// document a future aoe wrote.
pub const SCHEMA_VERSION: u32 = 1;

/// Subdirectory of the app dir bundle repos are cloned into.
///
/// Under the app dir on purpose: a workspace container mounts its per-user
/// volume at the app dir and nothing else, so a clone anywhere else is lost the
/// next time the container is recreated.
const REPOS_DIR: &str = "repos";

/// Credential store the `[git]` section writes, read by
/// `credential.helper store --file=...`.
const CREDENTIALS_FILE: &str = "git-credentials";

/// Where the `[git]` section's SSH key and its host keys go, read by the
/// `core.sshCommand` it configures. Under the app dir for the same reason the
/// credential store is: in a workspace container that is the only path that
/// survives the container being recreated.
const SSH_DIR: &str = "ssh";
const SSH_KEY_FILE: &str = "id_cityhall";
const SSH_KNOWN_HOSTS_FILE: &str = "known_hosts";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CityHallBundle {
    pub schema_version: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,

    /// Sparse aoe settings overrides: section -> field -> value.
    #[serde(default = "empty_object", skip_serializing_if = "is_empty_object")]
    pub settings: Value,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<BundleProject>,

    /// Git identity and credential. Spliced in by CityHall per user; never
    /// written by [`export`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitIdentity>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    /// Which aoe wrote the document. Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<String>,
}

/// A repo a workspace should have, addressed by remote so it is reproducible on
/// a machine that has never seen the admin's filesystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BundleProject {
    /// Registry name, and the directory name under `<app_dir>/repos`.
    pub name: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GitIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_email: Option<String>,
    /// Origin the credential is for, e.g. `https://github.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_token: Option<String>,
    /// Private key for `git@host:...` remotes, which a token cannot
    /// authenticate. Never passphrase-protected: nothing in a workspace can
    /// prompt for one, so CityHall refuses to store one that is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_private_key: Option<String>,
    /// `known_hosts` lines the key is used with. Goes in with the key or not at
    /// all: without them the connection would trust whatever answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_known_hosts: Option<String>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(|o| o.is_empty())
}

/// What [`apply`] did, so the CLI can print it and the serve path can log it.
///
/// Project failures are collected rather than fatal: one unreachable remote
/// must not cost the user every other repo.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Settings leaves merged into `config.toml`.
    pub settings_applied: usize,
    /// Projects cloned by this run (an existing checkout is left alone).
    pub cloned: Vec<String>,
    /// Projects added to the registry by this run.
    pub registered: Vec<String>,
    /// Projects already checked out and already registered, so this run had
    /// nothing to do. Tracked because a re-apply of an unchanged bundle
    /// legitimately does no work, and that is not the same as doing none.
    pub preserved: Vec<String>,
    /// Per-project failures, pre-formatted for display.
    pub failures: Vec<String>,
}

impl CityHallBundle {
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing the CityHall bundle")
    }

    pub fn from_toml(raw: &str) -> Result<Self> {
        let bundle: Self = toml::from_str(raw).context("parsing the CityHall bundle")?;
        if bundle.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported CityHall bundle schema_version {} (this aoe understands {SCHEMA_VERSION})",
                bundle.schema_version
            );
        }
        Ok(bundle)
    }
}

/// Build a bundle from this install's global config and project registry.
///
/// The project registry read is the **global** scope only: a workspace runs the
/// default profile, so a profile-scoped entry would not be visible there anyway.
pub fn export() -> Result<CityHallBundle> {
    let baseline = serde_json::to_value(Config::default())?;
    let current = serde_json::to_value(Config::load()?)?;

    let mut settings = empty_object();
    apply_changed_leaves(&mut settings, &baseline, &current);
    // `Config` has sections with no settings descriptors (`hooks`, `agents`,
    // `plugins`, ...). `validate_patch` rejects those outright, so drop them
    // here rather than shipping a document that cannot be applied.
    retain_schema_fields(&mut settings);
    // Host-specific paths (node binaries, socket paths) are meaningless in a
    // container, which is exactly what `local_only` marks.
    strip_local_only(&mut settings);
    strip_nulls(&mut settings);

    let mut projects_out = Vec::new();
    for project in projects::load_global()? {
        match crate::git::get_remote_url(Path::new(&project.path)) {
            Some(remote) => projects_out.push(BundleProject {
                name: project.name,
                remote: strip_userinfo(&remote),
                default_base_branch: project.default_base_branch,
            }),
            // Without a remote there is nothing a workspace could clone.
            None => tracing::warn!(
                project = %project.name,
                path = %project.path,
                "skipping project with no origin remote"
            ),
        }
    }

    Ok(CityHallBundle {
        schema_version: SCHEMA_VERSION,
        meta: Some(Meta {
            generated_by: Some(format!("aoe {}", env!("CARGO_PKG_VERSION"))),
        }),
        settings,
        projects: projects_out,
        git: None,
    })
}

/// Apply a bundle to this install: merge its settings, install its git identity,
/// then clone and register its projects.
///
/// Idempotent, because it runs on every workspace boot. An existing checkout is
/// left completely alone (a user's uncommitted work must survive a restart) and
/// an already-registered project is not re-added.
pub fn apply(bundle: &CityHallBundle) -> Result<ApplyReport> {
    if bundle.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported CityHall bundle schema_version {} (this aoe understands {SCHEMA_VERSION})",
            bundle.schema_version
        );
    }

    let mut report = ApplyReport::default();
    let app_dir = get_app_dir()?;

    report.settings_applied = apply_settings(&bundle.settings)?;

    if let Some(git) = &bundle.git {
        apply_git_identity(git, &app_dir)?;
    }

    apply_projects(&bundle.projects, &app_dir, &mut report);

    Ok(report)
}

/// Merge the sparse settings patch into the global `config.toml`, going through
/// the same validate-then-merge pair the settings PATCH handler uses.
fn apply_settings(settings: &Value) -> Result<usize> {
    let count = leaf_count(settings);
    if count == 0 {
        return Ok(0);
    }

    // Elevated: the bundle is admin-authored, so an elevation-gated field is
    // legitimately settable. `local_only` fields are still refused by the
    // validator itself.
    validate_patch(settings, Scope::Global, true)
        .map_err(|e| anyhow!("bundle settings rejected: {}", e.message()))?;

    update_config(|config| -> Result<()> {
        let mut merged = serde_json::to_value(&*config)?;
        merge_json(&mut merged, settings);
        // Build the new value before assigning: `update_config` writes whatever
        // it finds in `config` even when the closure returns an error, so a
        // failed deserialize must not leave a half-applied struct behind.
        let next: Config = serde_json::from_value(merged)?;
        *config = next;
        Ok(())
    })??;

    Ok(count)
}

/// Install `user.name` / `user.email`, then whichever credentials the bundle
/// carries: an HTTPS token, an SSH key, both, or neither.
///
/// Shells out to `git config --global` rather than editing `~/.gitconfig`
/// directly: idempotent, and no config parsing to get wrong.
fn apply_git_identity(git: &GitIdentity, app_dir: &Path) -> Result<()> {
    if let Some(name) = git.user_name.as_deref().filter(|s| !s.is_empty()) {
        git_config_global("user.name", name)?;
    }
    if let Some(email) = git.user_email.as_deref().filter(|s| !s.is_empty()) {
        git_config_global("user.email", email)?;
    }

    apply_https_credential(git, app_dir)?;
    apply_ssh_key(git, app_dir)
}

/// When a token is present, a `store` helper pointed at an owner-only file.
fn apply_https_credential(git: &GitIdentity, app_dir: &Path) -> Result<()> {
    let (Some(host), Some(username), Some(token)) = (
        git.credential_host.as_deref().filter(|s| !s.is_empty()),
        git.credential_username.as_deref().filter(|s| !s.is_empty()),
        git.credential_token.as_deref().filter(|s| !s.is_empty()),
    ) else {
        return Ok(());
    };

    let path = app_dir.join(CREDENTIALS_FILE);
    write_owner_only(&path, &credential_line(host, username, token))
        .with_context(|| format!("writing {}", path.display()))?;

    git_config_global(
        "credential.helper",
        &format!("store --file={}", path.display()),
    )
}

/// When an SSH key is present, write it and its host keys owner-only and point
/// git at both.
///
/// `core.sshCommand` rather than a `~/.ssh/config` entry: a workspace container
/// mounts its volume at the app dir and nothing else, so anything written under
/// `~` is gone the next time the container is recreated. It also keeps the key
/// scoped to git rather than to every `ssh` the user runs.
///
/// Both halves or neither. A key with no host keys to check against is exactly
/// what `StrictHostKeyChecking=yes` refuses to connect with, so installing one
/// without the other would only produce a confusing failure at clone time.
fn apply_ssh_key(git: &GitIdentity, app_dir: &Path) -> Result<()> {
    let (Some(key), Some(known_hosts)) = (
        git.ssh_private_key
            .as_deref()
            .filter(|s| !s.trim().is_empty()),
        git.ssh_known_hosts
            .as_deref()
            .filter(|s| !s.trim().is_empty()),
    ) else {
        return Ok(());
    };

    let dir = app_dir.join(SSH_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Written aside and renamed into place, both of them, before either
    // destination changes. `write_owner_only` truncates first, so writing
    // straight to the real paths on a re-apply would leave a new key beside an
    // old `known_hosts` if the second write failed, and an existing
    // `core.sshCommand` already points at both. A rename over a live path is
    // atomic, so nothing ever reads a half-written file.
    //
    // Two renames still are not one operation, so a failure between them leaves
    // a new key with the old host keys. That is a far smaller window than a
    // failure between two truncating writes, and closing it entirely would mean
    // a generation directory plus a config rewrite to activate it, which is more
    // machinery than a boot-time apply that retries next boot deserves.
    //
    // Both files end with exactly one newline: ssh rejects a private key file
    // whose last line is unterminated, and the sender is not required to have
    // fixed that.
    let key_path = dir.join(SSH_KEY_FILE);
    let known_hosts_path = dir.join(SSH_KNOWN_HOSTS_FILE);
    let staged_key = stage(&key_path, &line_terminated(key))?;
    let staged_known_hosts = stage(&known_hosts_path, &line_terminated(known_hosts))?;
    activate(&staged_key, &key_path)?;
    activate(&staged_known_hosts, &known_hosts_path)?;

    // Shell-quoted because git hands this to a shell, and the app dir is a path
    // the user chose. `IdentitiesOnly` so an agent's other keys are not offered
    // first, which a host that limits attempts would close the connection over.
    git_config_global(
        "core.sshCommand",
        &format!(
            "ssh -i {} -o IdentitiesOnly=yes -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes",
            shell_quote(&key_path),
            shell_quote(&known_hosts_path)
        ),
    )
}

/// Write `contents` owner-only to a sibling of `final_path`, and return where.
fn stage(final_path: &Path, contents: &str) -> Result<PathBuf> {
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    let staged = final_path.with_file_name(name);
    write_owner_only(&staged, contents).with_context(|| format!("writing {}", staged.display()))?;
    Ok(staged)
}

/// Move a staged file onto its real path.
fn activate(staged: &Path, final_path: &Path) -> Result<()> {
    std::fs::rename(staged, final_path)
        .with_context(|| format!("installing {}", final_path.display()))
}

/// `s` with trailing whitespace replaced by exactly one newline.
fn line_terminated(s: &str) -> String {
    format!("{}\n", s.trim_end())
}

/// A path as one POSIX shell word.
///
/// Single quotes alone are not enough: they do not protect a path that contains
/// one, and `/home/o'connor/...` is a perfectly ordinary home directory. Ending
/// the quoted run, escaping the apostrophe, and reopening is the standard way to
/// spell it.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r#"'"'"'"#))
}

/// One `.git-credentials` line: `https://user:token@host`.
///
/// Both userinfo halves are percent-encoded, so a token containing `@` or `/`
/// cannot corrupt the line (or silently authenticate against the wrong host).
fn credential_line(host: &str, username: &str, token: &str) -> String {
    let (scheme, authority) = host
        .split_once("://")
        .unwrap_or(("https", host.trim_end_matches('/')));
    let user = utf8_percent_encode(username, NON_ALPHANUMERIC);
    let pass = utf8_percent_encode(token, NON_ALPHANUMERIC);
    format!(
        "{scheme}://{user}:{pass}@{}\n",
        authority.trim_end_matches('/')
    )
}

/// Write a secret so it is owner-only from the moment it exists.
///
/// A plain `fs::write` creates the file with the umask (usually 0644) and leaves
/// the token world-readable until a follow-up chmod lands. This file is a
/// persisted credential store, so like `login_sessions.toml` it deliberately
/// survives shutdown rather than being swept with the `serve.*` files.
fn write_owner_only(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    // `mode` only applies when the file is created, and boot reruns this over an
    // existing file, so narrow the one already on disk as well.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())
}

/// Drop any `user[:password]@` from a remote URL.
///
/// A checkout's origin can embed a token (`https://u:tok@host/org/repo.git`),
/// and the exported bundle is written to a file, served over HTTP, and stored by
/// CityHall. Auth arrives separately in `[git]`, so the userinfo is not needed to
/// clone. SSH shorthand (`git@host:org/repo`) is left alone: there the `git@` is
/// the transport user, not a secret.
fn strip_userinfo(remote: &str) -> String {
    let Some((scheme, rest)) = remote.split_once("://") else {
        return remote.to_string();
    };
    // Only the authority may hold userinfo; a path is allowed to contain `@`.
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match path {
        Some(path) => format!("{scheme}://{authority}/{path}"),
        None => format!("{scheme}://{authority}"),
    }
}

fn git_config_global(key: &str, value: &str) -> Result<()> {
    let output = std::process::Command::new("git")
        // --replace-all: without it git refuses a key that already holds several
        // values, and a bundle apply would fail on a config it could have fixed.
        .args(["config", "--global", "--replace-all", key, value])
        .output()
        .with_context(|| format!("running git config --global {key}"))?;
    if !output.status.success() {
        bail!(
            "git config --global {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Clone each project that is missing, then register each one that is not in the
/// global registry yet.
fn apply_projects(wanted: &[BundleProject], app_dir: &Path, report: &mut ApplyReport) {
    if wanted.is_empty() {
        return;
    }

    let existing = projects::load_global().unwrap_or_default();
    let repos_dir = app_dir.join(REPOS_DIR);
    let profile = Config::load()
        .map(|c| c.default_profile)
        .unwrap_or_else(|_| "default".to_string());

    for project in wanted {
        if let Err(e) = check_project_name(&project.name) {
            report.failures.push(format!("{}: {e}", project.name));
            continue;
        }
        let dest = repos_dir.join(&project.name);
        let checkout_existed = dest.exists();

        if !checkout_existed {
            if let Err(e) = std::fs::create_dir_all(&repos_dir) {
                report.failures.push(format!(
                    "{}: creating {}: {e}",
                    project.name,
                    repos_dir.display()
                ));
                continue;
            }
            match crate::git::clone_repo(&project.remote, &dest, false) {
                Ok(()) => report.cloned.push(project.name.clone()),
                Err(e) => {
                    report
                        .failures
                        .push(format!("{}: clone failed: {e}", project.name));
                    continue;
                }
            }
        }

        let dest_str = dest.to_string_lossy().to_string();
        if existing
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&project.name) || p.path == dest_str)
        {
            // Already in place. Recorded rather than silently skipped, so a
            // re-apply that legitimately has nothing to do is distinguishable
            // from one where every project failed.
            if checkout_existed {
                report.preserved.push(project.name.clone());
            }
            continue;
        }

        let mut entry = Project::new(&project.name, &dest_str, ProjectScope::Global);
        entry.default_base_branch = project.default_base_branch.clone();
        match projects::add(&profile, ProjectScope::Global, entry, false) {
            Ok(_) => report.registered.push(project.name.clone()),
            Err(e) => report
                .failures
                .push(format!("{}: could not register: {e}", project.name)),
        }
    }
}

/// A bundle arrives over the network, and its project name becomes a directory
/// name, so it has to be a plain single path segment.
fn check_project_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("project name is empty");
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        bail!("project name '{name}' is not a single path segment");
    }
    Ok(())
}

/// Drop every `section.field` that has no settings descriptor.
fn retain_schema_fields(patch: &mut Value) {
    let known: HashSet<(String, String)> =
        schema().into_iter().map(|d| (d.section, d.field)).collect();
    let Some(root) = patch.as_object_mut() else {
        return;
    };
    root.retain(|section, value| {
        let Some(fields) = value.as_object_mut() else {
            return false;
        };
        fields.retain(|field, _| known.contains(&(section.clone(), field.clone())));
        !fields.is_empty()
    });
}

/// Drop `null` leaves: TOML cannot represent them, and a bundle sets values
/// rather than unsetting them.
fn strip_nulls(patch: &mut Value) {
    let Some(root) = patch.as_object_mut() else {
        return;
    };
    root.retain(|_, value| {
        let Some(fields) = value.as_object_mut() else {
            return false;
        };
        fields.retain(|_, v| !v.is_null());
        !fields.is_empty()
    });
}

fn leaf_count(patch: &Value) -> usize {
    patch
        .as_object()
        .map(|root| {
            root.values()
                .filter_map(|v| v.as_object())
                .map(|fields| fields.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Where a fetched bundle is cached, so a later boot can tell "CityHall is
/// unreachable and we have never been configured" from "unreachable but we
/// already are".
pub fn cache_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("cityhall-bundle.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_through_toml() {
        let bundle = CityHallBundle {
            schema_version: SCHEMA_VERSION,
            meta: Some(Meta {
                generated_by: Some("aoe 1.2.3".into()),
            }),
            settings: json!({"acp": {"default_agent": "claude-code"}}),
            projects: vec![BundleProject {
                name: "cityhall".into(),
                remote: "https://github.com/agent-of-empires/cityhall.git".into(),
                default_base_branch: Some("main".into()),
            }],
            git: None,
        };
        let raw = bundle.to_toml().unwrap();
        assert_eq!(CityHallBundle::from_toml(&raw).unwrap(), bundle);
    }

    /// The two SSH fields are the boundary CityHall writes across, so what
    /// matters is that they survive the round trip and that a bundle written
    /// before they existed still deserializes rather than failing a workspace's
    /// boot.
    #[test]
    fn ssh_fields_round_trip_and_are_optional() {
        let bundle = CityHallBundle {
            schema_version: SCHEMA_VERSION,
            // Not `Default::default()`: that leaves `settings` as JSON null,
            // which TOML has no representation for.
            settings: empty_object(),
            git: Some(GitIdentity {
                user_name: Some("someone".into()),
                ssh_private_key: Some(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----"
                        .into(),
                ),
                ssh_known_hosts: Some("github.com ssh-ed25519 AAAA".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let parsed = CityHallBundle::from_toml(&bundle.to_toml().unwrap()).unwrap();
        assert_eq!(parsed, bundle);

        let older =
            format!("schema_version = {SCHEMA_VERSION}\n\n[git]\nuser_name = \"someone\"\n");
        let git = CityHallBundle::from_toml(&older).unwrap().git.unwrap();
        assert_eq!(git.ssh_private_key, None);
        assert_eq!(git.ssh_known_hosts, None);
    }

    /// `core.sshCommand` goes to a shell, so a path holding an apostrophe has to
    /// survive it. `/home/o'connor` is an ordinary home directory, and single
    /// quotes on their own would end the quoted run in the middle of it and break
    /// every SSH git operation in that workspace.
    #[test]
    fn shell_quoting_survives_an_apostrophe_in_the_path() {
        assert_eq!(
            shell_quote(Path::new("/home/aoe/ssh/id")),
            "'/home/aoe/ssh/id'"
        );
        assert_eq!(
            shell_quote(Path::new("/home/o'connor/ssh/id")),
            r#"'/home/o'"'"'connor/ssh/id'"#
        );

        // What a shell actually makes of it: one word, spelled back exactly.
        let quoted = shell_quote(Path::new("/home/o'connor/a b/id"));
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {quoted}"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/home/o'connor/a b/id"
        );
    }

    #[test]
    fn a_future_schema_version_is_refused() {
        let raw = format!("schema_version = {}\n", SCHEMA_VERSION + 1);
        let err = CityHallBundle::from_toml(&raw).unwrap_err().to_string();
        assert!(err.contains("schema_version"), "{err}");
    }

    /// A section with no descriptors (`hooks`, `agents`, ...) would make
    /// `validate_patch` reject the whole document, so export must not emit one.
    #[test]
    fn unknown_sections_are_dropped() {
        let mut patch = json!({
            "acp": {"default_agent": "claude-code"},
            "hooks": {"pre_create": "echo hi"},
            "theme": {"not_a_field": 1},
        });
        retain_schema_fields(&mut patch);
        assert_eq!(patch, json!({"acp": {"default_agent": "claude-code"}}));
    }

    #[test]
    fn null_leaves_are_dropped() {
        let mut patch = json!({"session": {"cpu_limit": null, "confirm_delete": true}});
        strip_nulls(&mut patch);
        assert_eq!(patch, json!({"session": {"confirm_delete": true}}));
    }

    /// Whatever export produces has to survive the validator, or a bundle from
    /// a stock install is dead on arrival.
    #[test]
    fn an_exported_settings_patch_validates() {
        let baseline = serde_json::to_value(Config::default()).unwrap();
        let mut current = baseline.clone();
        current["acp"]["max_concurrent_workers"] = json!(7);

        let mut settings = empty_object();
        apply_changed_leaves(&mut settings, &baseline, &current);
        retain_schema_fields(&mut settings);
        strip_local_only(&mut settings);
        strip_nulls(&mut settings);

        assert_eq!(leaf_count(&settings), 1, "only the moved leaf: {settings}");
        validate_patch(&settings, Scope::Global, true).expect("export must validate");
    }

    #[test]
    fn a_bad_settings_key_is_rejected_by_name() {
        let err = apply_settings(&json!({"acp": {"nope": 1}}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("acp.nope"),
            "must name the offending key: {err}"
        );
    }

    #[test]
    fn credential_line_cases() {
        let cases = [
            // A token with `@` or `/` must not be able to corrupt the line.
            (
                ("https://github.com", "someone", "gh/p@ss"),
                "https://someone:gh%2Fp%40ss@github.com\n",
            ),
            // A bare host with no scheme still has to produce a usable line.
            (("github.com/", "u", "t"), "https://u:t@github.com\n"),
        ];
        for ((host, username, token), expected) in cases {
            assert_eq!(credential_line(host, username, token), expected, "{host}");
        }
    }

    /// An origin can embed a token, and the bundle is stored and served, so the
    /// userinfo must not travel with it.
    #[test]
    fn exported_remotes_carry_no_userinfo() {
        let cases = [
            (
                "https://u:ghp_secret@github.com/org/repo.git",
                "https://github.com/org/repo.git",
            ),
            (
                "https://github.com/org/repo.git",
                "https://github.com/org/repo.git",
            ),
            // A path is allowed to contain `@`; only the authority is stripped.
            (
                "https://github.com/org/re@po.git",
                "https://github.com/org/re@po.git",
            ),
            // SSH shorthand has no scheme, and its `git@` is the transport user.
            ("git@github.com:org/repo.git", "git@github.com:org/repo.git"),
            ("https://u:p@github.com", "https://github.com"),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_userinfo(input), expected, "{input}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_credential_file_is_owner_only_even_if_it_already_exists() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git-credentials");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_owner_only(&path, "fresh").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn project_names_must_be_one_path_segment() {
        assert!(check_project_name("cityhall").is_ok());
        for bad in ["", "..", "a/b", "a\\b"] {
            assert!(check_project_name(bad).is_err(), "{bad} must be refused");
        }
    }
}
