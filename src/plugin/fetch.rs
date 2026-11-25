//! Fetching an external plugin into a staging tree, ready to be moved into

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{PluginManifest, RuntimeSpec};

use crate::github::{GitHubClient, GitHubClientConfig, GitHubError, DEFAULT_USER_AGENT};

use super::source::PluginSource;

pub struct FetchedPlugin {
    staging: Option<Staging>,
    operation: Option<Arc<crate::session::StorageFlock>>,
    cleanup_safe: bool,
    pub tree: PathBuf,
    pub manifest: PluginManifest,
    pub manifest_bytes: Vec<u8>,
    pub tree_hash: String,
    publication_hash: String,
    pub source: PluginSource,
    pub requested_ref: Option<String>,
    pub resolved_commit: Option<String>,
    pub release_tag: Option<String>,
    pub asset_name: Option<String>,
    pub asset_sha256: Option<String>,
    pub(super) budget: super::execution::Budget,
}

struct Staging {
    directory: PathBuf,
    mount: PathBuf,
    marker: Option<PathBuf>,
    volume: Option<crate::process::StagingVolume>,
}

impl Staging {
    fn create_directory(plugins_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(plugins_root)?;
        let directory = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(plugins_root)
            .context("creating plugin staging dir")?
            .keep();
        Ok(Self {
            mount: directory.clone(),
            directory,
            marker: None,
            volume: None,
        })
    }

    fn root(&self) -> &Path {
        self.volume
            .as_ref()
            .map_or(self.mount.as_path(), |volume| volume.root())
    }

    fn create(plugins_root: &Path, limit: u64) -> Result<Self> {
        std::fs::create_dir_all(plugins_root)?;
        let app_root = plugins_root
            .parent()
            .ok_or_else(|| anyhow!("plugin staging has no application directory"))?;
        let marker = app_root.join(super::install::STAGING_MARKER);
        // This ordinary temporary directory is still empty and unmounted.
        let mut temporary = tempfile::Builder::new();
        temporary.prefix(".staging-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = temporary.tempdir_in(plugins_root)?;
        let mount = directory.path().join("volume");
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&mount)?;
        super::install::create_staging_marker(&marker, &mount)?;
        #[cfg(test)]
        test_gate::reach(&mount, "staging-acquisition");
        // From this point onward, no TempDir::drop may traverse the mount.
        let mut staging = Self {
            directory: directory.keep(),
            mount,
            marker: Some(marker),
            volume: None,
        };
        let acquisition = super::execution::catch_panic(|| {
            crate::process::StagingVolume::create(&staging.mount, limit)
        });
        match acquisition {
            Ok(volume) => {
                staging.volume = Some(volume);
                Ok(staging)
            }
            Err(error) => {
                if error
                    .downcast_ref::<crate::process::StagingAcquisitionUncertain>()
                    .is_some()
                    || error
                        .downcast_ref::<super::execution::RecoveryRequired>()
                        .is_some()
                {
                    std::mem::forget(staging);
                    return Err(super::execution::RecoveryRequired(format!(
                        "strict plugin staging acquisition failed: {error:#}"
                    ))
                    .into());
                }
                if let Err(cleanup) = staging.close() {
                    std::mem::forget(staging);
                    return Err(super::execution::RecoveryRequired(format!(
                        "strict plugin staging acquisition failed: {error:#}; cleanup unproven: {cleanup:#}"
                    )).into());
                }
                Err(error).context("acquiring strict plugin staging filesystem")
            }
        }
    }

    fn limit(&self, requested: u64) -> u64 {
        self.volume
            .as_ref()
            .map_or(requested, crate::process::StagingVolume::limit)
    }

    #[cfg(test)]
    fn volume(&self) -> &crate::process::StagingVolume {
        self.volume.as_ref().expect("live staging volume")
    }

    fn close(&mut self) -> Result<()> {
        if let Some(volume) = &mut self.volume {
            volume
                .close()
                .context("proving plugin staging volume cleanup")?;
        }
        self.volume.take();
        super::install::remove_owned_tree(&self.directory)
            .context("removing detached plugin staging directory")?;
        if let Some(marker) = &self.marker {
            std::fs::remove_file(marker).context("removing plugin staging marker")?;
            std::fs::File::open(marker.parent().expect("staging marker parent"))?.sync_all()?;
            super::install::unregister_staging(marker);
        }
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Cleanup is explicit. Unwinding must never detach an uncertain volume.
        if let Some(volume) = self.volume.take() {
            std::mem::forget(volume);
        }
    }
}

fn cleanup_staging(
    mut staging: Staging,
    operation: Option<Arc<crate::session::StorageFlock>>,
) -> Result<()> {
    let result = super::execution::catch_panic(|| staging.close());
    if let Err(error) = result {
        let result = Err(super::execution::RecoveryRequired(format!(
            "plugin staging cleanup unproven: {error:#}"
        ))
        .into());
        super::install::log_recovery::<()>(&result);
        std::mem::forget((staging, operation));
        result
    } else {
        Ok(())
    }
}

impl FetchedPlugin {
    pub(super) fn journals_publication(&self) -> bool {
        matches!(&self.source, PluginSource::Github { .. })
            && self
                .staging
                .as_ref()
                .is_some_and(|stage| stage.marker.is_none())
    }

    pub(super) fn begin_publication(&self) -> Result<()> {
        if let Some(marker) = self
            .staging
            .as_ref()
            .and_then(|staging| staging.marker.as_deref())
        {
            super::install::mark_staging_publication(marker)?;
        }
        Ok(())
    }

    pub(super) fn close(mut self) -> Result<()> {
        match self.staging.take() {
            Some(staging) => cleanup_staging(staging, self.operation.take()),
            None => Ok(()),
        }
    }

    pub(super) async fn cleanup(self) -> Result<()> {
        tokio::task::spawn_blocking(move || self.close())
            .await
            .context("plugin staging cleanup task failed")?
    }

    pub(super) fn copy_for_publication(&self, destination: &Path) -> Result<()> {
        // In particular, do not canonicalize a Linux /proc/PID/root staging path.
        let source = crate::session::AnchoredDir::open(&self.tree)?;
        let mut budget = CopyBudget::for_source(&self.source);
        copy_anchored_tree(&source, destination, &mut budget)?;
        if complete_tree_hash_with_budget(destination, CopyBudget::for_source(&self.source))?
            != self.publication_hash
        {
            bail!("plugin publication copy differs from the approved staging tree");
        }
        Ok(())
    }
}

impl Drop for FetchedPlugin {
    fn drop(&mut self) {
        if let Some(staging) = self.staging.take() {
            if !self.cleanup_safe {
                let result = Err(super::execution::RecoveryRequired(
                    "plugin download cancelled before asynchronous writes settled".to_owned(),
                )
                .into());
                super::install::log_recovery::<()>(&result);
                std::mem::forget((staging, self.operation.take()));
                return;
            }
            let operation = self.operation.take();
            // Blocking cleanup owns both resources even if its awaiter is cancelled.
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    drop(runtime.spawn_blocking(move || cleanup_staging(staging, operation)));
                }
                Err(_) => {
                    let _ = cleanup_staging(staging, operation);
                }
            }
        }
    }
}

pub(super) async fn fetch(
    source: &PluginSource,
    operation: Option<Arc<crate::session::StorageFlock>>,
) -> Result<FetchedPlugin> {
    let plugins_root = super::plugins_dir()?;
    let clone_url = source.github_clone_url();
    let source = source.clone();
    let stage_operation = operation.clone();
    let fetched = tokio::task::spawn_blocking(move || {
        let result = super::execution::catch_panic(|| stage(source, plugins_root, clone_url));
        if super::execution::needs_recovery(&result) {
            super::install::log_recovery(&result);
            std::mem::forget(stage_operation);
            result
        } else {
            result.map(|mut fetched| {
                fetched.operation = stage_operation;
                fetched
            })
        }
    })
    .await
    .context("plugin preparation task failed")??;

    if matches!(&fetched.source, PluginSource::Github { .. })
        && matches!(
            &fetched.manifest.runtime,
            Some(RuntimeSpec::ReleaseBinary { .. })
        )
    {
        // Keep staging and the lease until asynchronous file operations finish.
        await_asset_download(tokio::spawn(download_release_binary(fetched, operation))).await
    } else {
        Ok(fetched)
    }
}

async fn await_asset_download(
    task: tokio::task::JoinHandle<Result<FetchedPlugin>>,
) -> Result<FetchedPlugin> {
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(error).context("plugin asset download task failed"),
    }
}

fn stage(
    source: PluginSource,
    plugins_root: PathBuf,
    clone_url: Option<String>,
) -> Result<FetchedPlugin> {
    stage_with_limit(source, plugins_root, clone_url, STAGING_LIMIT)
}

#[cfg(test)]
pub(super) fn stage_remote_fixture(source: PluginSource, repo: &Path) -> Result<FetchedPlugin> {
    stage(
        source,
        super::plugins_dir()?,
        Some(format!("file://{}", repo.display())),
    )
}

fn stage_with_limit(
    source: PluginSource,
    plugins_root: PathBuf,
    clone_url: Option<String>,
    limit: u64,
) -> Result<FetchedPlugin> {
    stage_with_limits(source, plugins_root, clone_url, limit, STAGING_ENTRY_LIMIT)
}

fn stage_with_limits(
    source: PluginSource,
    plugins_root: PathBuf,
    clone_url: Option<String>,
    limit: u64,
    entry_limit: usize,
) -> Result<FetchedPlugin> {
    let sandbox = crate::session::Config::load()?
        .session
        .plugin_staging_sandbox;
    let budget = super::execution::Budget::new();
    let mut staging = match &source {
        PluginSource::Github { .. } if sandbox => Staging::create(&plugins_root, limit)?,
        PluginSource::Github { .. } | PluginSource::Local(_) => {
            Staging::create_directory(&plugins_root)?
        }
    };
    let limit = staging.limit(limit);
    let tree = staging.root().join("tree");

    let result = super::execution::catch_panic(|| {
        #[cfg(test)]
        test_gate::reach_observed(&staging.directory, &tree, "staging-ready");
        let (requested_ref, resolved_commit) = match &source {
            PluginSource::Github { reference, .. } => {
                let sha = git_clone_checkout(
                    clone_url
                        .as_deref()
                        .expect("github source yields a clone url"),
                    reference.as_deref(),
                    &tree,
                    &staging,
                    budget,
                )?;
                (source.reference().map(String::from), Some(sha))
            }
            PluginSource::Local(path) => {
                if !path.is_dir() {
                    bail!("local plugin source {} is not a directory", path.display());
                }
                copy_tree(path, &tree)?;
                (None, None)
            }
        };
        let (manifest, manifest_bytes) = read_manifest(&tree)?;
        if tree.join(super::integrity::BUILD_OUTPUT_DIR).exists() {
            bail!(
                "plugin source ships the reserved build-output directory {:?}; it must only be produced by build steps, not committed",
                super::integrity::BUILD_OUTPUT_DIR
            );
        }
        let tree_hash = super::integrity::tree_hash(&tree)?;
        let publication_hash = complete_tree_hash_with_budget(
            &tree,
            CopyBudget::for_source_with_limits(&source, limit, entry_limit),
        )?;
        #[cfg(test)]
        test_gate::reach_observed(&staging.directory, &tree, "source-prepared");
        Ok(FetchedPlugin {
            staging: None,
            operation: None,
            cleanup_safe: true,
            tree,
            manifest,
            manifest_bytes,
            tree_hash,
            publication_hash,
            source,
            requested_ref,
            resolved_commit,
            release_tag: None,
            asset_name: None,
            asset_sha256: None,
            budget,
        })
    });
    match result {
        Ok(mut fetched) => {
            fetched.staging = Some(staging);
            Ok(fetched)
        }
        Err(error) => {
            let error = if staging.volume.is_some() {
                staging_limit_error(error, limit)
            } else {
                error
            };
            if error
                .downcast_ref::<super::execution::RecoveryRequired>()
                .is_some()
            {
                std::mem::forget(staging);
            } else if let Err(cleanup_error) = staging.close() {
                std::mem::forget(staging);
                return Err(super::execution::RecoveryRequired(format!(
                    "plugin preparation failed: {error:#}; staging cleanup unproven: {cleanup_error:#}"
                )).into());
            }
            Err(error)
        }
    }
}

fn staging_limit_error(error: anyhow::Error, limit: u64) -> anyhow::Error {
    let full = error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.raw_os_error(),
                Some(libc::ENOSPC) | Some(libc::EDQUOT)
            )
        }) || cause
            .downcast_ref::<nix::errno::Errno>()
            .is_some_and(|error| {
                matches!(
                    *error,
                    nix::errno::Errno::ENOSPC | nix::errno::Errno::EDQUOT
                )
            })
    }) || error
        .downcast_ref::<GitFailed>()
        .is_some_and(GitFailed::is_quota_failure);
    if full {
        error.context(format!(
            "plugin staging filesystem limit ({limit} bytes) exceeded"
        ))
    } else {
        error
    }
}

/// A manifest is small; the cap keeps a hostile symlink or huge file from
/// being read before the tree hash rejects it.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

fn read_manifest(tree: &Path) -> Result<(PluginManifest, Vec<u8>)> {
    let root = crate::session::AnchoredDir::open(tree)?;
    let Some(bytes) = root
        .read_regular(Path::new("aoe-plugin.toml"), MAX_MANIFEST_BYTES)
        .with_context(|| format!("reading {}", tree.join("aoe-plugin.toml").display()))?
    else {
        bail!(
            "no regular aoe-plugin.toml of at most {MAX_MANIFEST_BYTES} bytes at {}; \
             this repository is not an installable plugin. \
             The GitHub `aoe-plugin` topic is also used by unrelated Age of Empires projects.",
            tree.join("aoe-plugin.toml").display()
        );
    };
    let text = std::str::from_utf8(&bytes).context("aoe-plugin.toml is not valid UTF-8")?;
    let manifest = PluginManifest::from_toml_str(text).map_err(|e| anyhow!("{e}"))?;
    Ok((manifest, bytes))
}

#[derive(Debug, thiserror::Error)]
#[error("git {command} failed: {stderr}")]
struct GitFailed {
    command: String,
    stderr: String,
}

impl GitFailed {
    fn is_quota_failure(&self) -> bool {
        let diagnostic = self.stderr.to_ascii_lowercase();
        diagnostic.contains("no space left on device") || diagnostic.contains("disk quota exceeded")
    }

    fn is_missing_clone_branch(&self, reference: &str) -> bool {
        !self.is_quota_failure()
            && self.stderr.lines().any(|line| {
                line.strip_prefix("fatal: Remote branch ")
                    .and_then(|line| line.strip_suffix(" not found in upstream origin"))
                    == Some(reference)
            })
    }
}

fn configure_git(cmd: &mut Command) {
    // Authentication, certificates and URL rewrites belong to the user's trusted
    // configuration. Only the opt-in native backend confines helper writes.
    // Do not let inherited repository selectors redirect this fresh checkout.
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SHALLOW_FILE",
        "GIT_CONFIG",
    ] {
        cmd.env_remove(key);
    }
    // The backend's temporary directory takes precedence over host TMP/TEMP.
    let temporary = cmd.get_envs().find_map(|(key, value)| {
        (key == "TMPDIR")
            .then_some(value)
            .flatten()
            .map(std::ffi::OsStr::to_os_string)
    });
    if let Some(temporary) = temporary {
        cmd.env("TMP", &temporary).env("TEMP", temporary);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "core.autocrlf=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn git_command(args: &[&str], cwd: &Path, staging: &Staging) -> Command {
    let mut cmd = match &staging.volume {
        Some(volume) => volume.git_command(),
        None => {
            let mut cmd = Command::new("git");
            cmd.env("TMPDIR", &staging.mount);
            cmd
        }
    };
    configure_git(&mut cmd);
    cmd.args(["-C"])
        .arg(cwd)
        .args(args)
        .current_dir(&staging.directory)
        .env("GIT_CEILING_DIRECTORIES", &staging.directory);
    cmd
}

fn git_failure(args: &[&str], output: &std::process::Output) -> anyhow::Error {
    GitFailed {
        command: args.join(" "),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
    .into()
}

fn run_git(
    args: &[&str],
    cwd: &Path,
    staging: &Staging,
    budget: super::execution::Budget,
) -> Result<String> {
    let mut cmd = git_command(args, cwd, staging);
    let output = budget
        .run(&mut cmd)
        .context("failed to run git (is it installed and on PATH?)")?;
    if !output.status.success() {
        return Err(git_failure(args, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Clone and checkout stdout is progress noise that no caller reads.
fn run_git_discarding_stdout(
    args: &[&str],
    cwd: &Path,
    staging: &Staging,
    budget: super::execution::Budget,
) -> Result<()> {
    let mut cmd = git_command(args, cwd, staging);
    let output = budget
        .run_discarding_stdout(&mut cmd)
        .context("failed to run git (is it installed and on PATH?)")?;
    if !output.status.success() {
        return Err(git_failure(args, &output));
    }
    Ok(())
}

pub fn ls_remote(url: &str, reference: Option<&str>) -> Result<String> {
    if let Some(r) = reference {
        if is_full_commit_sha(r) {
            return Ok(r.to_ascii_lowercase());
        }
    }
    ls_remote_with_operation(url, reference, super::install::begin_operation()?)
}

pub(super) fn ls_remote_with_operation(
    url: &str,
    reference: Option<&str>,
    operation: Arc<crate::session::StorageFlock>,
) -> Result<String> {
    // Credential helpers and trusted transport configuration can write, even
    // for a reference listing. Share preparation's backend and exclusion.
    let sandbox = crate::session::Config::load()?
        .session
        .plugin_staging_sandbox;
    let operation = Some(operation);
    let acquisition = super::execution::catch_panic(|| {
        let plugins = super::plugins_dir()?;
        if sandbox {
            Staging::create(&plugins, STAGING_LIMIT)
        } else {
            Staging::create_directory(&plugins)
        }
    });
    let staging = match acquisition {
        Ok(staging) => staging,
        Err(error) => {
            if error
                .downcast_ref::<super::execution::RecoveryRequired>()
                .is_some()
            {
                let result = Err(error);
                super::install::log_recovery::<String>(&result);
                std::mem::forget(operation);
                return result;
            }
            return Err(error);
        }
    };
    let limit = staging.limit(STAGING_LIMIT);
    let result = super::execution::catch_panic(|| {
        let target = reference.unwrap_or("HEAD");
        let peeled = format!("{target}^{{}}");
        let mut command = git_command(
            &["ls-remote", "--", url, target, &peeled],
            &staging.mount,
            &staging,
        );
        let output = super::execution::Budget::new().run(&mut command)?;
        if !output.status.success() {
            return Err(git_failure(&["ls-remote"], &output));
        }
        parse_ls_remote(&String::from_utf8_lossy(&output.stdout), target)
    });
    if super::execution::needs_recovery(&result) {
        super::install::log_recovery(&result);
        std::mem::forget((staging, operation));
    } else {
        cleanup_staging(staging, operation)?;
    }
    result.map_err(|error| staging_limit_error(error, limit))
}

pub async fn latest_release_tag(owner: &str, repo: &str) -> Result<Option<String>> {
    let client = GitHubClient::unauthenticated(GitHubClientConfig {
        api_base: github_api_base(),
        user_agent: DEFAULT_USER_AGENT.to_string(),
        timeout: Duration::from_secs(60),
    })?;
    match client.latest_release(owner, repo).await {
        Ok(release) => Ok(Some(release.tag_name)),
        Err(GitHubError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn is_full_commit_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Preserve lock pins: an annotated tag's peeled commit wins over the first match.
fn parse_ls_remote(out: &str, target: &str) -> Result<String> {
    let mut first = None;
    for line in out.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if name.ends_with("^{}") {
            return Ok(sha.trim().to_owned());
        }
        first.get_or_insert(sha.trim());
    }
    first
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("ref {target:?} not found on the remote"))
}

/// `core.autocrlf=false` keeps the checkout byte-identical so tree hashes match across platforms.
fn git_clone_checkout(
    url: &str,
    reference: Option<&str>,
    dest: &Path,
    staging: &Staging,
    budget: super::execution::Budget,
) -> Result<String> {
    // Native Git runs inside the volume's namespace; ordinary Git uses this path directly.
    let native_tree = staging.mount.join("tree");
    let mut clone = git_command(
        &[
            "clone",
            "--origin",
            "origin",
            "--depth",
            "1",
            "--no-checkout",
        ],
        &staging.mount,
        staging,
    );
    if let Some(reference) = reference {
        clone.args(["--branch", reference]);
    }
    clone.args(["--", url]).arg(&native_tree).env("LC_ALL", "C");
    let output = budget.run_discarding_stdout(&mut clone)?;
    let commit = if output.status.success() {
        run_git(&["rev-parse", "HEAD"], &native_tree, staging, budget)?
    } else {
        let error = git_failure(&["clone"], &output);
        if !reference.is_some_and(|reference| {
            error
                .downcast_ref::<GitFailed>()
                .is_some_and(|error| error.is_missing_clone_branch(reference))
        }) {
            return Err(error);
        }
        if dest.try_exists()? {
            super::install::remove_owned_tree(dest)
                .context("removing unresolved shallow Git checkout")?;
        }
        let mut clone = git_command(
            &["clone", "--origin", "origin", "--no-checkout", "--", url],
            &staging.mount,
            staging,
        );
        clone.arg(&native_tree);
        let output = budget.run_discarding_stdout(&mut clone)?;
        if !output.status.success() {
            return Err(git_failure(&["clone"], &output));
        }
        let requested = reference.expect("only unresolved references retry");
        let revision = if let Some(branch) = requested.strip_prefix("refs/heads/") {
            format!("refs/remotes/origin/{branch}^{{commit}}")
        } else {
            format!("{requested}^{{commit}}")
        };
        run_git(
            &["rev-parse", "--verify", "--end-of-options", &revision],
            &native_tree,
            staging,
            budget,
        )?
    };
    run_git_discarding_stdout(
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--detach",
            &commit,
            "--",
        ],
        &native_tree,
        staging,
        budget,
    )?;
    let sha = run_git(&["rev-parse", "HEAD"], &native_tree, staging, budget)?;
    #[cfg(test)]
    test_gate::reach_observed(&staging.directory, dest, "git-checkout");
    super::install::remove_owned_tree(&dest.join(".git"))
        .context("removing staged Git metadata")?;
    Ok(sha)
}

/// Symlinks are an error, not skipped, so the install hash agrees with `integrity::tree_hash`.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    // Resolve only the root spelling. Descendants must never follow symlinks.
    let root = std::fs::canonicalize(src)
        .with_context(|| format!("resolving plugin source {}", src.display()))?;
    let source = crate::session::AnchoredDir::open(&root)?;
    let mut budget = CopyBudget::unlimited();
    copy_anchored_tree(&source, dst, &mut budget)
}

struct CopyBudget {
    bytes: u64,
    entries: usize,
}

impl CopyBudget {
    fn unlimited() -> Self {
        Self {
            bytes: u64::MAX,
            entries: usize::MAX,
        }
    }

    fn for_source(source: &PluginSource) -> Self {
        Self::for_source_with_limits(source, STAGING_LIMIT, STAGING_ENTRY_LIMIT)
    }

    fn for_source_with_limits(source: &PluginSource, bytes: u64, entries: usize) -> Self {
        match source {
            PluginSource::Github { .. } => Self { bytes, entries },
            PluginSource::Local(_) => Self::unlimited(),
        }
    }
}

fn copy_anchored_tree(
    source: &crate::session::AnchoredDir,
    dst: &Path,
    budget: &mut CopyBudget,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(source.path())?;
    let (device, inode) = source.identity()?;
    let device: u64 = device as _;
    let inode: u64 = inode as _;
    if device != metadata.dev() || inode != metadata.ino() {
        bail!("plugin source directory changed while staging");
    }
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let names = source.read_dir_complete(Path::new(""), budget.entries.saturating_add(1))?;
    if names.len() > budget.entries {
        bail!("plugin staging entry budget exceeded");
    }
    budget.entries -= names.len();
    for name in names {
        if name == ".git" {
            continue;
        }
        let leaf = Path::new(&name);
        let from = source.path().join(leaf);
        let to = dst.join(leaf);
        let stat = source
            .entry_stat(leaf)?
            .ok_or_else(|| anyhow!("plugin source entry disappeared: {}", from.display()))?;
        #[cfg(test)]
        test_gate::reach(&from, "copy");
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFLNK => bail!(
                "plugin source contains a symlink ({}); symlinks are not allowed",
                from.display()
            ),
            libc::S_IFDIR => {
                let child = source.child(leaf)?;
                if child.identity()? != (stat.st_dev, stat.st_ino) {
                    bail!(
                        "plugin source directory changed while staging: {}",
                        from.display()
                    );
                }
                copy_anchored_tree(&child, &to, budget)?;
            }
            libc::S_IFREG => {
                let mut input = source.open_regular(leaf, usize::MAX)?.ok_or_else(|| {
                    anyhow!(
                        "plugin source file changed while staging: {}",
                        from.display()
                    )
                })?;
                let metadata = input.metadata()?;
                let device: u64 = stat.st_dev as _;
                let inode: u64 = stat.st_ino as _;
                if metadata.dev() != device || metadata.ino() != inode {
                    bail!(
                        "plugin source file changed while staging: {}",
                        from.display()
                    );
                }
                #[cfg(test)]
                test_gate::reach(&from, "copy-content");
                let mut output = std::fs::File::create(&to)?;
                let copied = std::io::copy(&mut (&mut input).take(budget.bytes), &mut output)
                    .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
                budget.bytes -= copied;
                if budget.bytes == 0 && input.read(&mut [0])? != 0 {
                    bail!("plugin staging budget exceeded");
                }
                output.set_permissions(metadata.permissions())?;
            }
            _ => bail!(
                "plugin source contains an unsupported special file ({})",
                from.display()
            ),
        }
    }
    std::fs::set_permissions(dst, metadata.permissions())?;
    Ok(())
}

/// Publication proof covers every path, directory, permission bit and file byte.
/// The consent tree hash deliberately remains the pre-extraction source hash.
fn complete_tree_hash(tree: &Path) -> Result<String> {
    complete_tree_hash_with_budget(
        tree,
        CopyBudget {
            bytes: STAGING_LIMIT,
            entries: STAGING_ENTRY_LIMIT,
        },
    )
}

fn complete_tree_hash_with_budget(tree: &Path, mut budget: CopyBudget) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    fn visit(
        root: &crate::session::AnchoredDir,
        digest: &mut Sha256,
        budget: &mut CopyBudget,
        buffer: &mut [u8; 64 * 1024],
    ) -> Result<()> {
        let metadata = std::fs::metadata(root.path())?;
        let (device, inode) = root.identity()?;
        let device: u64 = device as _;
        let inode: u64 = inode as _;
        if device != metadata.dev() || inode != metadata.ino() {
            bail!("plugin publication directory changed");
        }
        digest.update((metadata.mode() & 0o7777).to_le_bytes());
        let mut names = root.read_dir_complete(Path::new(""), budget.entries.saturating_add(1))?;
        if names.len() > budget.entries {
            bail!("plugin staging entry budget exceeded");
        }
        budget.entries -= names.len();
        names.sort_unstable();
        digest.update((names.len() as u64).to_le_bytes());
        for name in names {
            let leaf = Path::new(&name);
            let stat = root
                .entry_stat(leaf)?
                .context("plugin publication entry disappeared")?;
            let bytes = name.as_os_str().as_bytes();
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
            match stat.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    digest.update(b"d");
                    let child = root.child(leaf)?;
                    if child.identity()? != (stat.st_dev, stat.st_ino) {
                        bail!("plugin publication directory changed");
                    }
                    visit(&child, digest, budget, buffer)?;
                }
                libc::S_IFREG => {
                    digest.update(b"f");
                    let mut file = root
                        .open_regular(leaf, usize::MAX)?
                        .context("plugin publication file changed")?;
                    let metadata = file.metadata()?;
                    let device: u64 = stat.st_dev as _;
                    let inode: u64 = stat.st_ino as _;
                    if metadata.dev() != device || metadata.ino() != inode {
                        bail!("plugin publication file changed");
                    }
                    digest.update((metadata.mode() & 0o7777).to_le_bytes());
                    digest.update(metadata.len().to_le_bytes());
                    let mut remaining = metadata.len();
                    loop {
                        let count = file.read(buffer)?;
                        if count == 0 {
                            break;
                        }
                        let count_bytes = count as u64;
                        budget.bytes = budget
                            .bytes
                            .checked_sub(count_bytes)
                            .context("plugin staging budget exceeded")?;
                        remaining = remaining
                            .checked_sub(count_bytes)
                            .context("plugin publication file grew")?;
                        digest.update(&buffer[..count]);
                    }
                    if remaining != 0 {
                        bail!("plugin publication file shrank");
                    }
                }
                _ => bail!("plugin publication contains a symlink or special file"),
            }
        }
        Ok(())
    }
    let root = crate::session::AnchoredDir::open(tree)?;
    let mut digest = Sha256::new();
    visit(&root, &mut digest, &mut budget, &mut [0u8; 64 * 1024])?;
    Ok(sha256_label(&digest.finalize()))
}

pub(super) fn github_api_base() -> String {
    std::env::var("AOE_UPDATE_API_BASE")
        .unwrap_or_else(|_| crate::github::DEFAULT_GITHUB_API_BASE.to_string())
}

async fn download_release_binary(
    fetched: FetchedPlugin,
    operation: Option<Arc<crate::session::StorageFlock>>,
) -> Result<FetchedPlugin> {
    let mut fetched = fetched;
    let limit = fetched
        .staging
        .as_ref()
        .and_then(|staging| staging.volume.as_ref())
        .map_or(STAGING_LIMIT, crate::process::StagingVolume::limit);
    // A cancelled Tokio file write can outlive its future. Until flush returns,
    // cancellation must retain the mount and its mutation lock, not clean them.
    fetched.cleanup_safe = false;
    let download = async {
        let (owner, repo) = match &fetched.source {
            PluginSource::Github { owner, repo, .. } => (owner.as_str(), repo.as_str()),
            PluginSource::Local(_) => bail!("a release-binary worker requires a GitHub source"),
        };
        let Some(RuntimeSpec::ReleaseBinary {
            asset: asset_template,
            bin,
        }) = &fetched.manifest.runtime
        else {
            bail!("release binary runtime required");
        };
        let client = GitHubClient::unauthenticated(GitHubClientConfig {
            api_base: github_api_base(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            timeout: Duration::from_secs(60),
        })?;

        let release = match fetched.requested_ref.as_deref() {
            Some(tag) => client
                .release_by_tag(owner, repo, tag)
                .await
                .with_context(|| format!("no release tagged {tag:?} for {owner}/{repo}"))?,
            None => client
                .latest_release(owner, repo)
                .await
                .with_context(|| format!("no latest release for {owner}/{repo}"))?,
        };

        let wanted = render_asset_template(asset_template, &fetched.manifest.version);
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == wanted)
            .ok_or_else(|| {
                let available: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
                anyhow!(
                    "release {} has no asset {wanted:?} for this platform; available: [{}]",
                    release.tag_name,
                    available.join(", ")
                )
            })?;

        let staging = fetched
            .tree
            .parent()
            .ok_or_else(|| anyhow!("plugin staging tree has no parent directory"))?;
        let asset_path = staging.join(".asset-download");
        download_asset(&asset.browser_download_url, &asset_path, limit).await?;
        Ok::<_, anyhow::Error>((
            asset_path.to_path_buf(),
            asset.name.clone(),
            release.tag_name,
            bin.clone(),
        ))
    }
    .await;
    fetched.cleanup_safe = true;
    let (asset_path, name, tag, bin) = match download {
        Ok(download) => download,
        Err(error) => {
            fetched.cleanup().await?;
            return Err(staging_limit_error(error, limit));
        }
    };
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let mut fetched = fetched;
        let result = super::execution::catch_panic(|| {
            #[cfg(test)]
            test_gate::reach_observed(
                &fetched.staging.as_ref().expect("staged asset").directory,
                &fetched.tree,
                "asset",
            );
            let sha = sha256_file(&asset_path)?;
            install_asset_into(&fetched.tree, &name, bin.as_deref(), &asset_path)?;
            fetched.publication_hash = complete_tree_hash(&fetched.tree)?;
            fetched.release_tag = Some(tag);
            fetched.asset_name = Some(name);
            fetched.asset_sha256 = Some(sha);
            Ok(())
        });
        match result {
            Ok(()) => Ok(fetched),
            Err(error) => {
                let error = staging_limit_error(error, limit);
                if error
                    .downcast_ref::<super::execution::RecoveryRequired>()
                    .is_some()
                {
                    let result = Err(error);
                    super::install::log_recovery::<FetchedPlugin>(&result);
                    std::mem::forget(fetched);
                    std::mem::forget(_operation);
                    result
                } else {
                    fetched.close()?;
                    Err(error)
                }
            }
        }
    })
    .await
    .context("plugin asset preparation task failed")?
}

pub(super) fn render_asset_template<'a>(
    template: &'a str,
    version: &str,
) -> std::borrow::Cow<'a, str> {
    let mut rendered = None;
    let mut copied = 0;
    for (offset, _) in template.match_indices("${") {
        let rest = &template[offset..];
        let (token, value) = if rest.starts_with("${os}") {
            ("${os}", std::env::consts::OS)
        } else if rest.starts_with("${arch}") {
            ("${arch}", std::env::consts::ARCH)
        } else if rest.starts_with("${version}") {
            ("${version}", version)
        } else {
            continue;
        };
        let output = rendered.get_or_insert_with(|| String::with_capacity(template.len()));
        output.push_str(&template[copied..offset]);
        output.push_str(value);
        copied = offset + token.len();
    }
    match rendered {
        Some(mut output) => {
            output.push_str(&template[copied..]);
            std::borrow::Cow::Owned(output)
        }
        None => std::borrow::Cow::Borrowed(template),
    }
}

async fn download_asset(url: &str, dest: &Path, limit: u64) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let client = reqwest::Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(Duration::from_secs(300))
        .build()?;
    let mut response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} failed: HTTP {status}");
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let result: Result<()> = async {
        let mut remaining = limit;
        while let Some(chunk) = response.chunk().await? {
            let len = u64::try_from(chunk.len()).context("asset chunk size exceeds u64")?;
            if len > remaining {
                bail!("plugin staging budget exceeded");
            }
            remaining -= len;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("writing {url} to {}", dest.display()))?;
        }
        Ok(())
    }
    .await;
    let flush = file
        .flush()
        .await
        .with_context(|| format!("writing {url} to {}", dest.display()));
    result?;
    flush
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file =
        std::fs::File::open(path).with_context(|| format!("hashing {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("hashing {}", path.display()))?;
        if read == 0 {
            return Ok(sha256_label(&hasher.finalize()));
        }
        hasher.update(&buf[..read]);
    }
}

fn sha256_label(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha256_label(&sha2::Sha256::digest(bytes))
}

fn install_asset_into(
    tree: &Path,
    asset_name: &str,
    bin: Option<&str>,
    asset: &Path,
) -> Result<()> {
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        let bin_rel =
            bin.ok_or_else(|| anyhow!("a release-binary archive asset must set `bin`"))?;
        let source =
            std::fs::File::open(asset).with_context(|| format!("opening {}", asset.display()))?;
        let bin_rel = checked_asset_path(Path::new(bin_rel))?;
        match std::fs::symlink_metadata(tree.join(&bin_rel)) {
            Ok(_) => bail!(
                "release asset {asset_name:?} would replace the plugin source entry {}",
                tree.join(&bin_rel).display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("checking release binary source path"),
        }
        extract_asset_archive(tree, asset_name, source, Some(&bin_rel), STAGING_LIMIT)?;
        ensure_executable(tree, &bin_rel)
    } else {
        let name = checked_asset_path(Path::new(bin.unwrap_or(asset_name)))?;
        let root = crate::session::AnchoredDir::open(tree)?;
        let mut remaining = remaining_tree_entries(&root, STAGING_ENTRY_LIMIT)?;
        if let Some(parent) = name.parent().filter(|p| !p.as_os_str().is_empty()) {
            ensure_asset_directories(&root, parent, &mut remaining)?;
        }
        take_entry(&mut remaining)?;
        let Some(mut output) = root.create_new_regular(&name)? else {
            bail!(
                "release asset {asset_name:?} would replace the plugin source file {}",
                tree.join(&name).display()
            );
        };
        let mut input =
            std::fs::File::open(asset).with_context(|| format!("opening {}", asset.display()))?;
        std::io::copy(&mut input, &mut output)
            .with_context(|| format!("writing {}", tree.join(&name).display()))?;
        set_asset_mode(&output, 0o755)
    }
}

const STAGING_LIMIT: u64 = 1024 * 1024 * 1024;
pub(crate) const STAGING_ENTRY_LIMIT: usize = 65_536;
const ARCHIVE_METADATA_LIMIT: u64 = 1024 * 1024;

fn take_entry(remaining: &mut usize) -> Result<()> {
    *remaining = remaining
        .checked_sub(1)
        .context("plugin staging entry budget exceeded")?;
    Ok(())
}

fn remaining_tree_entries(
    root: &crate::session::AnchoredDir,
    mut remaining: usize,
) -> Result<usize> {
    fn count(root: &crate::session::AnchoredDir, remaining: &mut usize) -> Result<()> {
        let names = root.read_dir_complete(Path::new(""), remaining.saturating_add(1))?;
        if names.len() > *remaining {
            bail!("plugin staging entry budget exceeded");
        }
        *remaining -= names.len();
        for name in names {
            let leaf = Path::new(&name);
            let stat = root
                .entry_stat(leaf)?
                .context("plugin staging entry disappeared")?;
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                count(&root.child(leaf)?, remaining)?;
            }
        }
        Ok(())
    }
    count(root, &mut remaining)?;
    Ok(remaining)
}

fn ensure_asset_directories(
    root: &crate::session::AnchoredDir,
    relative: &Path,
    remaining: &mut usize,
) -> Result<()> {
    let mut current = root.child(Path::new(""))?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            bail!("asset directory path is not normalized");
        };
        let leaf = Path::new(name);
        if current.entry_stat(leaf)?.is_none() {
            take_entry(remaining)?;
            current.ensure_dir(leaf)?;
        }
        current = current.child(leaf)?;
    }
    Ok(())
}

struct BoundedReader<'a, R> {
    inner: R,
    remaining: u64,
    metadata_remaining: &'a std::cell::Cell<Option<u64>>,
}

impl<R: Read> Read for BoundedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let metadata = self.metadata_remaining.get();
        let remaining = self.remaining.min(metadata.unwrap_or(u64::MAX));
        if remaining == 0 {
            let mut extra = [0];
            return match self.inner.read(&mut extra)? {
                0 => Ok(0),
                _ if self.remaining == 0 => {
                    Err(std::io::Error::other("plugin staging budget exceeded"))
                }
                _ => Err(std::io::Error::other(
                    "plugin archive metadata budget exceeded",
                )),
            };
        }
        let limit = remaining.min(buf.len() as u64) as usize;
        let read = self.inner.read(&mut buf[..limit])?;
        self.remaining -= read as u64;
        if let Some(metadata) = metadata {
            self.metadata_remaining.set(Some(metadata - read as u64));
        }
        Ok(read)
    }
}

/// Assets may only add files: a replacement would change the manifest or tree
/// after the consent fingerprint was computed over the source snapshot.
fn extract_asset_archive(
    tree: &Path,
    asset_name: &str,
    source: std::fs::File,
    executable: Option<&Path>,
    limit: u64,
) -> Result<()> {
    extract_asset_archive_with_limits(
        tree,
        asset_name,
        source,
        executable,
        limit,
        STAGING_ENTRY_LIMIT,
    )
}

fn extract_asset_archive_with_limits(
    tree: &Path,
    asset_name: &str,
    source: std::fs::File,
    executable: Option<&Path>,
    limit: u64,
    entry_limit: usize,
) -> Result<()> {
    let root = crate::session::AnchoredDir::open(tree)?;
    let mut remaining = remaining_tree_entries(&root, entry_limit)?;
    let mut headers_remaining = entry_limit;
    let metadata_remaining = std::cell::Cell::new(None);
    let decoder = BoundedReader {
        inner: flate2::read::GzDecoder::new(source),
        remaining: limit,
        metadata_remaining: &metadata_remaining,
    };
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive
        .entries()
        .with_context(|| format!("unpacking {asset_name}"))?;
    loop {
        // Tar reads PAX and GNU metadata before yielding the entry.
        metadata_remaining.set(Some(ARCHIVE_METADATA_LIMIT));
        let next = entries.next();
        metadata_remaining.set(None);
        let Some(entry) = next else {
            break;
        };
        take_entry(&mut headers_remaining)?;
        let mut entry = entry.with_context(|| format!("unpacking {asset_name}"))?;
        let kind = entry.header().entry_type();
        // A global PAX header describes later entries and names no path.
        if kind == tar::EntryType::XGlobalHeader {
            if entry.size() > ARCHIVE_METADATA_LIMIT {
                bail!("plugin archive metadata budget exceeded");
            }
            std::io::copy(&mut entry, &mut std::io::sink())
                .with_context(|| format!("unpacking {asset_name}"))?;
            continue;
        }
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
        let entry_path = entry
            .path()
            .with_context(|| format!("reading an entry name from {asset_name}"))?
            .into_owned();
        let normalized = normalized_asset_path(&entry_path)?;
        if normalized.as_os_str().is_empty() {
            // `tar -C dist .` stores a "./" entry for the archive root.
            if kind.is_dir() {
                std::io::copy(&mut entry, &mut std::io::sink())
                    .with_context(|| format!("unpacking {asset_name}"))?;
                continue;
            }
            bail!("plugin path {entry_path:?} is empty");
        }
        let rel = checked_asset_path(&normalized)?;
        if kind.is_dir() {
            ensure_asset_directories(&root, &rel, &mut remaining)
                .with_context(|| format!("unpacking {asset_name}"))?;
            std::io::copy(&mut entry, &mut std::io::sink())
                .with_context(|| format!("unpacking {asset_name}"))?;
        } else if kind.is_file() {
            if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                ensure_asset_directories(&root, parent, &mut remaining)
                    .with_context(|| format!("unpacking {asset_name}"))?;
            }
            take_entry(&mut remaining)?;
            let Some(mut output) = root.create_new_regular(&rel)? else {
                bail!(
                    "release asset {asset_name:?} would replace the plugin source entry {}",
                    tree.join(&rel).display()
                );
            };
            std::io::copy(&mut entry, &mut output)
                .with_context(|| format!("unpacking {asset_name}"))?;
            // The binary must stay readable for the verifying reopen below,
            // which is why its executable mode is set through this descriptor.
            let mode = match executable {
                Some(bin) if bin == rel => mode | 0o755,
                _ => mode,
            };
            set_asset_mode(&output, mode)?;
        } else {
            bail!(
                "release asset {asset_name:?} contains an unsupported entry ({})",
                rel.display()
            );
        }
    }
    Ok(())
}

fn normalized_asset_path(rel: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            _ => bail!(
                "plugin path {rel:?} must be a relative path inside the plugin (no absolute path or `..`)"
            ),
        }
    }
    Ok(normalized)
}

fn checked_asset_path(rel: &Path) -> Result<PathBuf> {
    let normalized = normalized_asset_path(rel)?;
    let Some(first) = normalized.components().next() else {
        bail!("plugin path {rel:?} is empty");
    };
    let first = first.as_os_str();
    if first == ".git" || first == super::integrity::BUILD_OUTPUT_DIR {
        bail!("plugin path {rel:?} uses the reserved {first:?} namespace");
    }
    Ok(normalized)
}

fn set_asset_mode(file: &std::fs::File, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (file, mode);
    Ok(())
}

/// Open through the anchored root so a planted symlink is never chmodded.
fn ensure_executable(tree: &Path, rel: &Path) -> Result<()> {
    let root = crate::session::AnchoredDir::open(tree)?;
    let Some(file) = root.open_regular(rel, usize::MAX)? else {
        bail!(
            "expected binary {} missing after extraction",
            tree.join(rel).display()
        );
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode();
        set_asset_mode(&file, mode | 0o755)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_without_native_tools(directory: &Path) -> crate::session::test_support::EnvGuard {
        let guard = crate::session::test_support::EnvGuard::read_lock();
        let inherited = std::env::var_os("PATH").expect("test PATH");
        std::fs::create_dir(directory).unwrap();
        for name in ["git", "sh", "ps"] {
            let executable = std::env::split_paths(&inherited)
                .map(|path| path.join(name))
                .find(|path| path.is_file())
                .unwrap_or_else(|| panic!("{name} missing from test PATH"));
            std::os::unix::fs::symlink(executable.canonicalize().unwrap(), directory.join(name))
                .unwrap();
        }
        guard.and_set("PATH", directory)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn default_remote_staging_and_ref_checks_work_without_native_tools() {
        use crate::git::test_support::run_git;
        use crate::session::test_support::{isolate_app_dir, EnvGuard};
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        assert!(
            !crate::session::Config::load()
                .unwrap()
                .session
                .plugin_staging_sandbox
        );
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("acme/stage.git");
        write_publication_manifest(&repo);
        std::fs::write(repo.join("payload"), b"remote snapshot").unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "fixture"]);
        run_git(&repo, &["tag", "-a", "v1", "-m", "release"]);
        let commit = run_git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        let clone_base = format!("file://{}", fixture.path().display());
        let _configuration = EnvGuard::set(&[
            ("AOE_GITHUB_CLONE_BASE", clone_base.as_str()),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]);
        let _path = path_without_native_tools(&fixture.path().join("path"));
        let source = PluginSource::parse("gh:acme/stage@v1").unwrap();
        let url = source.github_clone_url().unwrap();
        assert_eq!(ls_remote(&url, None).unwrap(), commit);
        assert_eq!(ls_remote(&url, Some("v1")).unwrap(), commit);
        let operation = super::super::install::begin_operation().unwrap();
        assert_eq!(
            ls_remote_with_operation(&url, Some("v1"), operation.clone()).unwrap(),
            commit
        );
        let mut fetched = fetch(&source, Some(operation)).await.unwrap();
        assert_eq!(fetched.resolved_commit.as_deref(), Some(commit.as_str()));
        let staging = fetched.staging.as_ref().unwrap();
        let directory = staging.directory.clone();
        assert!(staging.volume.is_none());
        assert!(staging.marker.is_none());
        assert!(!fetched.tree.join(".git").exists());
        assert!(super::super::install::begin_operation().is_err());
        std::fs::write(repo.join("payload"), b"changed source").unwrap();
        let asset = directory.join("worker");
        std::fs::write(&asset, b"worker").unwrap();
        install_asset_into(&fetched.tree, "worker", Some("bin/worker"), &asset).unwrap();
        fetched.publication_hash = complete_tree_hash(&fetched.tree).unwrap();
        fetched.begin_publication().unwrap();
        let destination = fixture.path().join("published");
        fetched.copy_for_publication(&destination).unwrap();
        assert_eq!(
            std::fs::read(destination.join("payload")).unwrap(),
            b"remote snapshot"
        );
        assert_eq!(
            std::fs::read(destination.join("bin/worker")).unwrap(),
            b"worker"
        );
        fetched.cleanup().await.unwrap();
        assert!(!directory.exists());
        for (bytes, entries, expected) in [
            (1, STAGING_ENTRY_LIMIT, "plugin staging budget exceeded"),
            (STAGING_LIMIT, 1, "plugin staging entry budget exceeded"),
        ] {
            let error = stage_with_limits(
                source.clone(),
                crate::plugin::plugins_dir().unwrap(),
                Some(url.clone()),
                bytes,
                entries,
            )
            .err()
            .expect("remote snapshot limit");
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        let consent = super::super::install::preview_install("gh:acme/stage@v1", false)
            .await
            .unwrap();
        super::super::install::apply_install(
            "gh:acme/stage@v1",
            &consent.fingerprint,
            false,
            super::super::install::OperationLog::Inherit,
        )
        .await
        .unwrap();
        let plugins = crate::plugin::plugins_dir().unwrap();
        assert_eq!(
            std::fs::read(plugins.join("acme.stage/payload")).unwrap(),
            b"remote snapshot"
        );
        assert!(crate::session::Config::load()
            .unwrap()
            .plugins
            .contains_key("acme.stage"));
        assert!(std::fs::read_dir(&plugins).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")));
        assert!(!crate::session::get_app_dir()
            .unwrap()
            .join(super::super::install::STAGING_MARKER)
            .exists());
        assert!(super::super::install::begin_operation().is_ok());
    }

    #[test]
    #[serial_test::serial]
    #[cfg(target_os = "linux")]
    fn explicit_strict_staging_refuses_missing_native_tools_without_fallback() {
        use crate::git::test_support::run_git;
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|config| config.session.plugin_staging_sandbox = true)
            .unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repository");
        write_publication_manifest(&repo);
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "fixture"]);
        let _path = path_without_native_tools(&fixture.path().join("path"));
        let url = format!("file://{}", repo.display());
        let plugins = crate::plugin::plugins_dir().unwrap();
        let error = stage(
            PluginSource::parse("gh:acme/stage").unwrap(),
            plugins.clone(),
            Some(url.clone()),
        )
        .err()
        .expect("strict preparation must not fall back to ordinary Git");
        assert!(format!("{error:#}").contains("acquiring strict plugin staging filesystem"));
        let error = ls_remote(&url, None).unwrap_err();
        assert!(format!("{error:#}").contains("acquiring strict plugin staging filesystem"));
        assert_eq!(std::fs::read_dir(plugins).unwrap().count(), 0);
        assert!(!crate::session::get_app_dir()
            .unwrap()
            .join(super::super::install::STAGING_MARKER)
            .exists());
        assert!(super::super::install::begin_operation().is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn staging_backend_selection_propagates_config_errors() {
        let _home = crate::session::test_support::isolate_app_dir();
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        write_publication_manifest(&source);
        let plugins = crate::plugin::plugins_dir().unwrap();
        let operation = super::super::install::begin_operation().unwrap();
        std::fs::write(crate::session::config::config_path().unwrap(), "[session\n").unwrap();
        let expected = format!("{:#}", crate::session::Config::load().unwrap_err());
        for source in [
            PluginSource::Local(source),
            PluginSource::parse("gh:acme/stage").unwrap(),
        ] {
            let url = source.github_clone_url();
            let error = stage(source, plugins.clone(), url)
                .err()
                .expect("invalid config");
            assert_eq!(format!("{error:#}"), expected);
        }
        let error = ls_remote_with_operation("unused://", None, operation).unwrap_err();
        assert_eq!(format!("{error:#}"), expected);
        assert!(!plugins.exists() || std::fs::read_dir(plugins).unwrap().next().is_none());
    }

    fn publication_fixture(tree: PathBuf) -> FetchedPlugin {
        let (manifest, manifest_bytes) = read_manifest(&tree).unwrap();
        FetchedPlugin {
            staging: None,
            operation: None,
            cleanup_safe: true,
            tree_hash: super::super::integrity::tree_hash(&tree).unwrap(),
            publication_hash: complete_tree_hash(&tree).unwrap(),
            source: PluginSource::Local(tree.clone()),
            tree,
            manifest,
            manifest_bytes,
            requested_ref: None,
            resolved_commit: None,
            release_tag: None,
            asset_name: None,
            asset_sha256: None,
            budget: super::super::execution::Budget::new(),
        }
    }

    fn write_publication_manifest(tree: &Path) {
        std::fs::create_dir_all(tree).unwrap();
        std::fs::write(
            tree.join("aoe-plugin.toml"),
            "id = \"acme.stage\"\nname = \"Stage\"\nversion = \"1.0.0\"\napi_version = 2\n",
        )
        .unwrap();
    }

    #[test]
    fn publication_copy_covers_directory_modes_and_post_snapshot_assets() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("source");
        write_publication_manifest(&tree);
        std::fs::create_dir(tree.join("empty")).unwrap();
        std::fs::set_permissions(tree.join("empty"), std::fs::Permissions::from_mode(0o751))
            .unwrap();
        let mut fetched = publication_fixture(tree.clone());
        let source_hash = fetched.tree_hash.clone();
        let asset = fixture.path().join("asset");
        std::fs::write(&asset, b"worker").unwrap();
        install_asset_into(&tree, "worker", Some("bin/worker"), &asset).unwrap();
        fetched.publication_hash = complete_tree_hash(&tree).unwrap();
        let destination = fixture.path().join("published");
        fetched.copy_for_publication(&destination).unwrap();
        assert_eq!(fetched.tree_hash, source_hash);
        assert_eq!(
            std::fs::read(destination.join("bin/worker")).unwrap(),
            b"worker"
        );
        assert_eq!(
            std::fs::metadata(destination.join("empty"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
        assert_eq!(
            std::fs::metadata(destination.join("bin/worker"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        std::fs::set_permissions(tree.join("empty"), std::fs::Permissions::from_mode(0o750))
            .unwrap();
        assert!(fetched
            .copy_for_publication(&fixture.path().join("changed"))
            .is_err());
    }

    #[test]
    fn archive_binary_must_not_chmod_preexisting_source() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        write_publication_manifest(&tree);
        std::fs::write(tree.join("worker"), b"source").unwrap();
        std::fs::set_permissions(tree.join("worker"), std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let before = complete_tree_hash(&tree).unwrap();
        let asset = fixture.path().join("archive.tgz");
        let encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&asset).unwrap(),
            flate2::Compression::default(),
        );
        let archive = tar::Builder::new(encoder);
        archive.into_inner().unwrap().finish().unwrap();
        let error = install_asset_into(&tree, "archive.tgz", Some("worker"), &asset).unwrap_err();
        assert!(format!("{error:#}").contains("would replace"));
        assert_eq!(complete_tree_hash(&tree).unwrap(), before);
    }

    #[test]
    fn archive_entry_limit_counts_existing_and_implicit_directories() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("source"), b"source").unwrap();
        let asset = fixture.path().join("archive.tgz");
        let encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&asset).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "a/b/worker", std::io::empty())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        let error = extract_asset_archive_with_limits(
            &tree,
            "archive.tgz",
            std::fs::File::open(asset).unwrap(),
            None,
            1024 * 1024,
            3,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("entry budget exceeded"));
        assert!(tree.join("a/b").is_dir());
        assert!(!tree.join("a/b/worker").exists());
        assert_eq!(std::fs::read(tree.join("source")).unwrap(), b"source");
    }

    #[test]
    #[serial_test::serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    fn staging_volume_high_ratio_archive_hits_filesystem_limit_and_cleans() {
        use crate::git::test_support::run_git;
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|config| config.session.plugin_staging_sandbox = true)
            .unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        write_publication_manifest(&source);
        run_git(&source, &["init", "-q"]);
        run_git(&source, &["add", "."]);
        run_git(&source, &["commit", "-qm", "fixture"]);
        let plugins = fixture.path().join("plugins");
        let fetched = stage_with_limit(
            PluginSource::parse("gh:acme/archive").unwrap(),
            plugins.clone(),
            Some(format!("file://{}", source.display())),
            32 * 1024 * 1024,
        )
        .unwrap();
        let asset = fetched.tree.parent().unwrap().join("archive.tgz");
        let encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&asset).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(64 * 1024 * 1024);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                "worker",
                std::io::repeat(0).take(64 * 1024 * 1024),
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        assert!(std::fs::metadata(&asset).unwrap().len() < 1024 * 1024);
        let error =
            install_asset_into(&fetched.tree, "archive.tgz", Some("worker"), &asset).unwrap_err();
        assert!(
            format!("{:#}", staging_limit_error(error, 32 * 1024 * 1024))
                .contains("filesystem limit")
        );
        fetched.close().unwrap();
        assert!(!fixture
            .path()
            .join(super::super::install::STAGING_MARKER)
            .exists());
        assert_eq!(std::fs::read_dir(plugins).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn asset_task_panics_reach_the_job_panic_boundary() {
        use futures_util::FutureExt;

        let task: tokio::task::JoinHandle<Result<FetchedPlugin>> =
            tokio::spawn(async { panic!("private-panic-payload") });
        let panic = std::panic::AssertUnwindSafe(await_asset_download(task))
            .catch_unwind()
            .await;
        assert!(panic.is_err());
    }

    #[test]
    fn parse_ls_remote_prefers_the_peeled_tag_and_a_pinned_sha_skips_the_remote() {
        let peeled = "1111111111111111111111111111111111111111\trefs/tags/v1\n\
                      2222222222222222222222222222222222222222\trefs/tags/v1^{}";
        assert_eq!(
            parse_ls_remote(peeled, "v1").unwrap(),
            "2222222222222222222222222222222222222222"
        );
        let homonymous =
            format!("{peeled}\n3333333333333333333333333333333333333333\trefs/heads/v1");
        for target in ["v1", "refs/tags/v1"] {
            assert_eq!(
                parse_ls_remote(&homonymous, target).unwrap(),
                "2222222222222222222222222222222222222222"
            );
        }
        assert_eq!(
            parse_ls_remote(
                "3333333333333333333333333333333333333333\trefs/heads/v1",
                "refs/heads/v1",
            )
            .unwrap(),
            "3333333333333333333333333333333333333333"
        );
        assert_eq!(
            parse_ls_remote("3333333333333333333333333333333333333333\tHEAD", "HEAD").unwrap(),
            "3333333333333333333333333333333333333333"
        );
        assert!(parse_ls_remote("", "nope").is_err());

        let pinned = "abcdef0123456789abcdef0123456789abcdef01";
        assert_eq!(ls_remote("unused://", Some(pinned)).unwrap(), pinned);
    }

    #[test]
    #[serial_test::serial]
    fn ls_remote_peels_annotated_tag_to_commit() {
        use crate::git::test_support::run_git;

        let _home = crate::session::test_support::isolate_app_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        run_git(path, &["init", "-q"]);
        std::fs::write(path.join("f"), b"x").unwrap();
        run_git(path, &["add", "f"]);
        run_git(path, &["commit", "-qm", "c"]);
        run_git(path, &["tag", "-a", "v1", "-m", "release"]);

        let sha_of = |rev: &str| run_git(path, &["rev-parse", rev]).trim().to_string();
        let commit = sha_of("HEAD");
        let tag_object = sha_of("v1");
        assert_ne!(
            commit, tag_object,
            "annotated tag object should differ from the commit"
        );

        let resolved = ls_remote(path.to_str().unwrap(), Some("v1")).unwrap();
        assert_eq!(
            resolved, commit,
            "ls_remote should return the peeled commit, not the tag object"
        );
        assert_ne!(resolved, tag_object);
    }

    #[test]
    #[cfg(unix)]
    fn detached_staging_close_removes_read_only_subtrees() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let directory = fixture.path().join("plugins/.staging-read-only");
        let mount = directory.join("volume");
        let nested = mount.join("tree/read-only");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("file"), b"owned staging content").unwrap();
        for path in [&nested, &mount.join("tree"), &mount, &directory] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let marker = fixture.path().join(super::super::install::STAGING_MARKER);
        super::super::install::create_staging_marker(&marker, &mount).unwrap();
        let mut staging = Staging {
            directory: directory.clone(),
            mount,
            marker: Some(marker.clone()),
            volume: None,
        };

        staging.close().unwrap();

        assert!(!directory.exists());
        assert!(!marker.exists());
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires private user/mount namespaces and bubblewrap"
    )]
    async fn staging_volume_secondary_branch_preserves_trusted_user_git_configuration() {
        use crate::git::test_support::run_git;
        use crate::session::test_support::{isolate_app_dir, EnvGuard};
        let _home = isolate_app_dir();
        crate::session::update_config(|config| config.session.plugin_staging_sandbox = true)
            .unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repository");
        write_publication_manifest(&repo);
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "default"]);
        run_git(&repo, &["checkout", "-qb", "feature/review"]);
        std::fs::write(repo.join("branch"), b"secondary").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "secondary"]);
        let commit = run_git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        run_git(&repo, &["tag", "-a", "release", "-m", "release"]);
        run_git(&repo, &["checkout", "-q", "-"]);
        let tag_commit = run_git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        assert_ne!(tag_commit, commit);
        run_git(
            &repo,
            &["tag", "-a", "feature/review", "-m", "homonymous tag"],
        );
        let global = fixture.path().join("gitconfig");
        let rewrite = format!("url.file://{}.insteadOf", repo.display());
        let url = "https://plugin-fixture.invalid/repository";
        run_git(
            &repo,
            &["config", "--file", global.to_str().unwrap(), &rewrite, url],
        );
        run_git(
            &repo,
            &[
                "config",
                "--file",
                global.to_str().unwrap(),
                "http.extraHeader",
                "X-AOE-Fixture: preserved",
            ],
        );
        run_git(
            &repo,
            &[
                "config",
                "--file",
                global.to_str().unwrap(),
                "credential.helper",
                "!printf trusted-helper",
            ],
        );
        run_git(
            &repo,
            &[
                "config",
                "--file",
                global.to_str().unwrap(),
                "clone.defaultRemoteName",
                "upstream",
            ],
        );
        let _configuration = EnvGuard::set(&[
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]);
        assert_eq!(ls_remote(url, Some("feature/review")).unwrap(), tag_commit);
        let operation = super::super::install::begin_operation().unwrap();
        assert_eq!(
            ls_remote_with_operation(url, Some("feature/review"), operation.clone()).unwrap(),
            tag_commit
        );
        let staging = Arc::new(
            Staging::create(&crate::plugin::plugins_dir().unwrap(), 16 * 1024 * 1024).unwrap(),
        );
        let budget = super::super::execution::Budget::new();
        assert_eq!(
            super::run_git(
                &["config", "--get", "http.extraHeader"],
                &staging.mount,
                &staging,
                budget
            )
            .unwrap(),
            "X-AOE-Fixture: preserved"
        );
        assert_eq!(
            super::run_git(
                &["config", "--get", "credential.helper"],
                &staging.mount,
                &staging,
                budget
            )
            .unwrap(),
            "!printf trusted-helper"
        );
        assert_eq!(
            super::run_git(
                &["config", "--get", "clone.defaultRemoteName"],
                &staging.mount,
                &staging,
                budget
            )
            .unwrap(),
            "upstream"
        );
        let tree = staging.volume().root().join("tree");
        for (reference, expected, shallow) in [
            (None, tag_commit.as_str(), true),
            (Some("feature/review"), commit.as_str(), true),
            (Some("refs/heads/feature/review"), commit.as_str(), false),
            (Some("release"), commit.as_str(), true),
            (Some("refs/tags/release"), commit.as_str(), false),
            (Some(commit.as_str()), commit.as_str(), false),
            (Some(tag_commit.as_str()), tag_commit.as_str(), false),
            (Some("refs/tags/feature/review"), tag_commit.as_str(), false),
        ] {
            let mut gate = test_gate::install(&staging.directory, "git-checkout");
            let task_staging = staging.clone();
            let reference = reference.map(str::to_owned);
            let destination = tree.clone();
            let task = tokio::task::spawn_blocking(move || {
                git_clone_checkout(
                    url,
                    reference.as_deref(),
                    &destination,
                    &task_staging,
                    budget,
                )
            });
            tokio::time::timeout(Duration::from_secs(5), &mut gate.entered)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(tree.join(".git/shallow").exists(), shallow);
            let configuration = std::fs::read_to_string(tree.join(".git/config")).unwrap();
            assert!(configuration.contains("[remote \"origin\"]"));
            assert!(!configuration.contains("[remote \"upstream\"]"));
            gate.release();
            assert_eq!(task.await.unwrap().unwrap(), expected);
            assert_eq!(tree.join("branch").exists(), expected == commit);
            std::fs::remove_dir_all(&tree).unwrap();
        }
        let mut staging = Arc::try_unwrap(staging).ok().expect("finished Git task");
        staging.close().unwrap();
        drop(operation);
        assert!(super::super::install::begin_operation().is_ok());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn safe_acquisition_failure_keeps_live_marker_until_proven_cleanup() {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|config| config.session.plugin_staging_sandbox = true)
            .unwrap();
        let operation = super::super::install::begin_operation().unwrap();
        let root = crate::session::get_app_dir().unwrap();
        let mut gate = test_gate::install(&root, "staging-acquisition");
        let plugins = crate::plugin::plugins_dir().unwrap();
        let task_operation = operation.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _operation = task_operation;
            stage_with_limit(
                PluginSource::parse("gh:acme/acquisition").unwrap(),
                plugins,
                Some("unused://".to_owned()),
                0,
            )
        });
        tokio::time::timeout(Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        assert!(root.join(super::super::install::STAGING_MARKER).is_file());
        assert!(super::super::install::check_recovery_evidence().is_ok());
        assert!(super::super::install::begin_operation().is_err());
        gate.release();
        let result = task.await.unwrap();
        assert!(result.is_err());
        assert!(!super::super::execution::needs_recovery(&result));
        drop(operation);
        assert!(!root.join(super::super::install::STAGING_MARKER).exists());
        assert_eq!(
            std::fs::read_dir(crate::plugin::plugins_dir().unwrap())
                .unwrap()
                .count(),
            0
        );
        assert!(super::super::install::begin_operation().is_ok());
    }

    #[test]
    fn renders_platform_tokens() {
        let platform = format!(
            "w-{}-{}-1.2.3.tar.gz",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        for (template, version, expected) in [
            (
                "w-${os}-${arch}-${version}.tar.gz",
                "1.2.3",
                platform.as_str(),
            ),
            ("literal-${unknown}", "1.2.3", "literal-${unknown}"),
            ("µ-${version}-${version}", "1.2.3", "µ-1.2.3-1.2.3"),
            ("${version}", "${os}", "${os}"),
        ] {
            assert_eq!(render_asset_template(template, version), expected);
        }
    }

    #[test]
    fn checked_asset_path_rejects_escapes_and_reserved_namespaces() {
        assert_eq!(
            checked_asset_path(Path::new("bin/worker")).unwrap(),
            Path::new("bin/worker")
        );
        assert_eq!(
            checked_asset_path(Path::new("./worker")).unwrap(),
            Path::new("worker")
        );
        for bad in [
            "../../.bashrc",
            "/etc/passwd",
            "a/../../b",
            "",
            ".git/config",
            ".aoe-build/worker",
        ] {
            assert!(
                checked_asset_path(Path::new(bad)).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn raw_asset_is_written_executable() {
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("download");
        std::fs::write(&downloaded, b"#!/bin/sh\n").unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        install_asset_into(&tree, "thing", Some("thing"), &downloaded).unwrap();
        let path = tree.join("thing");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "should be executable, mode {mode:o}");
        }
    }

    #[test]
    fn copy_tree_skips_git_and_rejects_symlinks() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("aoe-plugin.toml"), b"x").unwrap();
        std::fs::create_dir(src.path().join(".git")).unwrap();
        std::fs::write(src.path().join(".git").join("config"), b"y").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let into = dst.path().join("tree");
        copy_tree(src.path(), &into).unwrap();
        assert!(into.join("aoe-plugin.toml").exists());
        assert!(!into.join(".git").exists());

        std::os::unix::fs::symlink("aoe-plugin.toml", src.path().join("link")).unwrap();
        let err = copy_tree(src.path(), &dst.path().join("tree2"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "got: {err}");
    }

    #[tokio::test]
    async fn anchored_copy_refuses_special_entries_and_replacement_races() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for replacement in ["fifo", "symlink", "directory-fifo", "directory-symlink"] {
            let src = tempfile::tempdir().unwrap();
            let dst = tempfile::tempdir().unwrap();
            let entry = src.path().join("entry");
            let directory = replacement.starts_with("directory");
            if directory {
                std::fs::create_dir(&entry).unwrap();
            } else {
                std::fs::write(&entry, b"original").unwrap();
            }
            let mut gate = test_gate::install(&entry.canonicalize().unwrap(), "copy");
            let root = src.path().to_owned();
            let into = dst.path().join("tree");
            let task = tokio::task::spawn_blocking(move || copy_tree(&root, &into));
            tokio::time::timeout(Duration::from_secs(5), &mut gate.entered)
                .await
                .unwrap()
                .unwrap();
            if directory {
                std::fs::remove_dir(&entry).unwrap();
            } else {
                std::fs::remove_file(&entry).unwrap();
            }
            if replacement.ends_with("fifo") {
                nix::unistd::mkfifo(&entry, nix::sys::stat::Mode::S_IRUSR).unwrap();
            } else {
                symlink(dst.path(), &entry).unwrap();
            }
            gate.release();
            assert!(tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_err());
        }
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let fifo = src.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(copy_tree(src.path(), &dst.path().join("fifo-tree")).is_err());
        std::fs::remove_file(fifo).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(src.path().join("socket")).unwrap();
        assert!(copy_tree(src.path(), &dst.path().join("socket-tree")).is_err());
        drop(socket);
        std::fs::remove_file(src.path().join("socket")).unwrap();
        std::fs::create_dir(src.path().join("empty")).unwrap();
        std::fs::write(src.path().join("worker"), b"executable").unwrap();
        std::fs::set_permissions(
            src.path().join("worker"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        symlink(src.path(), dst.path().join("root-link")).unwrap();
        std::fs::create_dir(dst.path().join("subdir")).unwrap();
        copy_tree(
            &dst.path().join("subdir/../root-link"),
            &dst.path().join("valid"),
        )
        .unwrap();
        assert!(dst.path().join("valid/empty").is_dir());
        assert_eq!(
            std::fs::metadata(dst.path().join("valid/worker"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn release_asset_blocking_tail_owns_staging_lease_and_provenance() {
        use crate::git::test_support::run_git;
        use crate::session::test_support::{isolate_app_dir, EnvGuard};
        use std::sync::{Arc, Mutex};
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _home = isolate_app_dir();
        let repositories = tempfile::tempdir().unwrap();
        let repo = repositories.path().join("acme/bin.git");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("aoe-plugin.toml"), "id = \"acme.bin\"\nname = \"Binary\"\nversion = \"1.0.0\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"release-binary\"\nasset = \"worker\"\nbin = \"worker\"\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "fixture"]);
        run_git(&repo, &["tag", "v1"]);
        let bytes = Arc::new(Mutex::new(b"#!/bin/sh\nexit 0\n".to_vec()));
        let download = bytes.clone();
        let pause = Arc::new(Mutex::new(
            None::<(
                tokio::sync::oneshot::Sender<()>,
                tokio::sync::oneshot::Receiver<()>,
            )>,
        ));
        let pause_download = pause.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release_base = base.clone();
        let app = axum::Router::new()
            .route("/repos/acme/bin/releases/tags/{tag}", axum::routing::get(move |axum::extract::Path(tag): axum::extract::Path<String>| {
                let name = if tag == "v2" { "worker.tgz" } else { "worker" };
                let release = serde_json::json!({"tag_name":tag,"assets":[{"name":name,"browser_download_url":format!("{release_base}/asset")}]}).to_string();
                async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], release) }
            }))
            .route("/asset", axum::routing::get(move || {
                let download = download.clone();
                let pause = pause_download.lock().unwrap().take();
                async move {
                    if let Some((entered, release)) = pause {
                        let _ = entered.send(());
                        let _ = release.await;
                    }
                    download.lock().unwrap().clone()
                }
            }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _env = EnvGuard::set(&[
            ("AOE_UPDATE_API_BASE", base.as_str()),
            (
                "AOE_GITHUB_CLONE_BASE",
                repositories.path().to_str().unwrap(),
            ),
        ]);
        let _path = path_without_native_tools(&repositories.path().join("path"));
        let source = PluginSource::parse("gh:acme/bin@v1").unwrap();
        let plugins = crate::plugin::plugins_dir().unwrap();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        *pause.lock().unwrap() = Some((entered, released));
        let mut preparation = test_gate::install(&plugins, "source-prepared");
        let operation = crate::plugin::install::begin_operation().unwrap();
        let staged_source = source.clone();
        let task = tokio::spawn(async move { fetch(&staged_source, Some(operation)).await });
        preparation.wait_for_preparation().await;
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(crate::plugin::install::begin_operation().is_err());
        release.send(()).unwrap();
        let operation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(operation) = crate::plugin::install::begin_operation() {
                    break operation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!std::fs::read_dir(&plugins).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".staging-")
        }));
        drop(operation);
        let mut gate = test_gate::install(&plugins, "asset");
        let mut preparation = test_gate::install(&plugins, "source-prepared");
        let operation = crate::plugin::install::begin_operation().unwrap();
        let staged_source = source.clone();
        let task = tokio::spawn(async move { fetch(&staged_source, Some(operation)).await });
        preparation.wait_for_preparation().await;
        let tree = tokio::time::timeout(Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        // The same single-worker runtime continues serving unrelated API requests.
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let api = axum::Router::new()
            .route(
                "/api/plugins",
                axum::routing::get(crate::server::api::plugins::list_plugins),
            )
            .route(
                "/api/plugins/jobs/{job_id}",
                axum::routing::get(crate::server::api::plugins::plugin_job_status),
            )
            .with_state(state);
        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", api_listener.local_addr().unwrap());
        let api_server = tokio::spawn(async move { axum::serve(api_listener, api).await.unwrap() });
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            reqwest::get(format!("{api_base}/api/plugins")),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.status().is_success());
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            reqwest::get(format!("{api_base}/api/plugins/jobs/absent")),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(tree.exists());
        assert!(crate::plugin::install::begin_operation().is_err());
        gate.release();
        let operation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(operation) = crate::plugin::install::begin_operation() {
                    break operation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!tree.exists(), "cleanup must precede lease release");
        drop(operation);
        let fetched = fetch(&source, None).await.unwrap();
        let expected_hash = sha256_hex(&bytes.lock().unwrap());
        assert_eq!(
            fetched.asset_sha256.as_deref(),
            Some(expected_hash.as_str())
        );
        assert_eq!(fetched.release_tag.as_deref(), Some("v1"));
        assert_eq!(fetched.asset_name.as_deref(), Some("worker"));
        assert_eq!(
            std::fs::read(fetched.tree.join("worker")).unwrap(),
            *bytes.lock().unwrap()
        );
        let source_hash = fetched.tree_hash.clone();
        fetched.cleanup().await.unwrap();
        let before_injection =
            stage(source.clone(), plugins.clone(), source.github_clone_url()).unwrap();
        assert_eq!(source_hash, before_injection.tree_hash);
        before_injection.cleanup().await.unwrap();
        let consent = crate::plugin::install::preview_install("gh:acme/bin@v1", false)
            .await
            .unwrap();
        assert!(consent.fingerprint.contains(&expected_hash));
        *bytes.lock().unwrap() = b"changed asset".to_vec();
        let result = crate::plugin::install::apply_install(
            "gh:acme/bin@v1",
            &consent.fingerprint,
            false,
            crate::plugin::install::OperationLog::Inherit,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("changed since"));
        assert!(!crate::session::Config::load()
            .unwrap()
            .plugins
            .contains_key("acme.bin"));
        assert!(!plugins.join("acme.bin").exists());
        let manifest = std::fs::read_to_string(repo.join("aoe-plugin.toml"))
            .unwrap()
            .replace("asset = \"worker\"", "asset = \"worker.tgz\"");
        std::fs::write(repo.join("aoe-plugin.toml"), manifest).unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "archive"]);
        run_git(&repo, &["tag", "v2"]);
        let archive_source = PluginSource::parse("gh:acme/bin@v2").unwrap();
        let operation = crate::plugin::install::begin_operation().unwrap();
        let error = fetch(&archive_source, Some(operation))
            .await
            .err()
            .expect("malformed archive");
        assert!(format!("{error:#}").contains("unpacking worker.tgz"));
        assert!(crate::plugin::install::begin_operation().is_ok());
        assert!(!crate::session::Config::load()
            .unwrap()
            .plugins
            .contains_key("acme.bin"));
        assert!(!plugins.join("acme.bin").exists());
        api_server.abort();
        server.abort();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn git_budget_and_clone_failure_never_retry() {
        const CHILD: &str = "_AOE_INTERNAL_GIT_BUDGET_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "plugin::fetch::tests::git_budget_and_clone_failure_never_retry",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let ran_one = output
                .stdout
                .windows(b"running 1 test".len())
                .any(|window| window == b"running 1 test");
            assert!(
                output.status.success() && ran_one,
                "isolated Git budget test failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        static EXPIRED: AtomicBool = AtomicBool::new(false);
        EXPIRED.store(false, Ordering::SeqCst);
        struct ExpireOnDrop;
        impl Drop for ExpireOnDrop {
            fn drop(&mut self) {
                EXPIRED.store(true, Ordering::SeqCst);
            }
        }
        let _expire = ExpireOnDrop;
        let fixture = tempfile::tempdir().unwrap();
        let git = fixture.path().join("git");
        let staging = Staging::create(&fixture.path().join("plugins"), 16 * 1024 * 1024).unwrap();
        let calls = staging.volume().root().join("calls");
        let ready = staging.volume().root().join("ready");
        let native_calls = staging.mount.join("calls");
        let native_ready = staging.mount.join("ready");
        std::fs::write(
            &git,
            format!(
                "#!/bin/sh\nprintf 'call\\n' >> '{}'\n: > '{}'\nwhile :; do sleep 0.01; done\n",
                native_calls.display(),
                native_ready.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _path = crate::session::test_support::path_prepended(fixture.path());
        let budget = super::super::execution::Budget::controlled_for_test(&EXPIRED);
        let destination = staging.volume().root().join("tree");
        let task = tokio::task::spawn_blocking(move || {
            let error = git_clone_checkout("fixture", Some("v1"), &destination, &staging, budget);
            (error, staging)
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        EXPIRED.store(true, Ordering::SeqCst);
        let (result, mut staging) = task.await.unwrap();
        let error = result.unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::execution::Exhausted>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        std::fs::write(&calls, "").unwrap();
        std::fs::write(&git, format!(
            "#!/bin/sh\nprintf 'call\\n' >> '{}'\nprintf ordinary-checkout-failure >&2\nexit 1\n", native_calls.display()
        )).unwrap();
        EXPIRED.store(false, Ordering::SeqCst);
        let error = git_clone_checkout(
            "fixture",
            Some("v1"),
            &staging.volume().root().join("tree"),
            &staging,
            budget,
        )
        .unwrap_err();
        assert!(error.to_string().contains("ordinary-checkout-failure"));
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        for diagnostic in [
            "No space left on device",
            "Disk quota exceeded",
            "fatal: Remote branch v1 not found in upstream origin\\nNo space left on device",
        ] {
            std::fs::write(&calls, "").unwrap();
            std::fs::write(
                &git,
                format!(
                    "#!/bin/sh\nprintf 'call\\n' >> '{}'\nprintf '{diagnostic}' >&2\nexit 1\n",
                    native_calls.display()
                ),
            )
            .unwrap();
            let error = git_clone_checkout(
                "fixture",
                Some("v1"),
                &staging.volume().root().join("tree"),
                &staging,
                budget,
            )
            .unwrap_err();
            assert!(
                format!("{:#}", staging_limit_error(error, 16 * 1024 * 1024))
                    .contains("filesystem limit")
            );
            assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        }
        staging.close().unwrap();
        let mut success = Command::new("sh");
        success
            .args(["-c", "printf stdout; printf stderr >&2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = budget.run(&mut success).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
    }

    #[test]
    fn archive_asset_preserves_bytes_hash_and_errors() {
        use std::io::Write;
        let raw = b"#!/bin/sh\nexit 0\n";
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(raw.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive
            .append_data(&mut header, "worker", &raw[..])
            .unwrap();
        let mut encoder = archive.into_inner().unwrap();
        encoder.flush().unwrap();
        let bytes = encoder.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("worker.tar.gz");
        std::fs::write(&downloaded, &bytes).unwrap();
        let ok_tree = dir.path().join("ok");
        std::fs::create_dir(&ok_tree).unwrap();
        install_asset_into(&ok_tree, "worker.tar.gz", Some("worker"), &downloaded).unwrap();
        assert_eq!(std::fs::read(ok_tree.join("worker")).unwrap(), raw);
        assert_ne!(sha256_hex(&bytes), sha256_hex(raw));
        let broken = dir.path().join("broken.tgz");
        std::fs::write(&broken, b"bad").unwrap();
        let broken_tree = dir.path().join("broken");
        std::fs::create_dir(&broken_tree).unwrap();
        assert!(
            install_asset_into(&broken_tree, "broken.tgz", Some("worker"), &broken)
                .unwrap_err()
                .to_string()
                .contains("unpacking")
        );
        let archive_path = dir.path().join("worker.tgz");
        std::fs::write(&archive_path, &bytes).unwrap();
        let unset_tree = dir.path().join("unset");
        std::fs::create_dir(&unset_tree).unwrap();
        assert!(
            install_asset_into(&unset_tree, "worker.tgz", None, &archive_path)
                .unwrap_err()
                .to_string()
                .contains("must set `bin`")
        );
        let missing_tree = dir.path().join("missing");
        std::fs::create_dir(&missing_tree).unwrap();
        assert!(
            install_asset_into(&missing_tree, "worker.tgz", Some("missing"), &archive_path)
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
    }

    #[cfg(unix)]
    fn gzip(entries: &[(&str, Option<&[u8]>, u32)]) -> Vec<u8> {
        use std::io::Write;
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (name, data, mode) in entries {
            let mut header = tar::Header::new_gnu();
            match data {
                Some(data) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(data.len() as u64);
                    header.set_mode(*mode);
                    header.set_cksum();
                    archive.append_data(&mut header, name, &data[..]).unwrap();
                }
                None => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(*mode);
                    header.set_cksum();
                    archive
                        .append_data(&mut header, name, std::io::empty())
                        .unwrap();
                }
            }
        }
        let mut encoder = archive.into_inner().unwrap();
        encoder.flush().unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn local_staging_ignores_remote_limits_and_freezes_source() {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|config| config.session.plugin_staging_sandbox = true)
            .unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        write_publication_manifest(&source);
        std::fs::write(source.join("payload"), b"local snapshot").unwrap();
        let plugins = crate::plugin::plugins_dir().unwrap();
        let operation = super::super::install::begin_operation().unwrap();
        let mut fetched = stage_with_limits(
            PluginSource::Local(source.clone()),
            plugins.clone(),
            None,
            1,
            1,
        )
        .unwrap();
        fetched.operation = Some(operation);
        let staging = fetched.staging.as_ref().unwrap();
        assert!(staging.volume.is_none());
        assert!(staging.marker.is_none());
        assert!(staging.directory.starts_with(&plugins));
        assert!(!crate::session::get_app_dir()
            .unwrap()
            .join(super::super::install::STAGING_MARKER)
            .exists());
        assert!(super::super::install::begin_operation().is_err());
        std::fs::write(source.join("payload"), b"changed source").unwrap();
        let destination = fixture.path().join("published");
        fetched.copy_for_publication(&destination).unwrap();
        assert_eq!(
            std::fs::read(destination.join("payload")).unwrap(),
            b"local snapshot"
        );
        for (bytes, entries, expected) in [
            (1, usize::MAX, "plugin staging budget exceeded"),
            (u64::MAX, 1, "plugin staging entry budget exceeded"),
        ] {
            let error =
                complete_tree_hash_with_budget(&fetched.tree, CopyBudget { bytes, entries })
                    .unwrap_err();
            assert!(format!("{error:#}").contains(expected));
        }
        let asset = fixture.path().join("remote.tgz");
        std::fs::write(&asset, gzip(&[("worker", Some(b"remote"), 0o755)])).unwrap();
        for (bytes, entries, expected) in [
            (1, STAGING_ENTRY_LIMIT, "plugin staging budget exceeded"),
            (STAGING_LIMIT, 1, "plugin staging entry budget exceeded"),
        ] {
            let error = extract_asset_archive_with_limits(
                &fetched.tree,
                "remote.tgz",
                std::fs::File::open(&asset).unwrap(),
                None,
                bytes,
                entries,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected));
        }
        std::fs::write(fetched.tree.join("payload"), b"changed staging").unwrap();
        assert!(fetched
            .copy_for_publication(&fixture.path().join("changed"))
            .is_err());
        fetched.close().unwrap();
        assert_eq!(std::fs::read_dir(&plugins).unwrap().count(), 0);
        assert!(super::super::install::begin_operation().is_ok());
        assert_eq!(
            std::fs::read(source.join("payload")).unwrap(),
            b"changed source"
        );
    }

    #[test]
    fn remote_copy_bounds_total_bytes_and_entries() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("first"), b"1234").unwrap();
        std::fs::write(source.join("nested/second"), b"5678").unwrap();
        let anchored = crate::session::AnchoredDir::open(&source).unwrap();
        for (name, bytes, entries, expected) in [
            ("exact", 8, 3, None),
            ("bytes", 7, 3, Some("plugin staging budget exceeded")),
            (
                "entries",
                8,
                2,
                Some("plugin staging entry budget exceeded"),
            ),
        ] {
            let dest = work.path().join(name);
            let mut budget = CopyBudget { bytes, entries };
            let result = copy_anchored_tree(&anchored, &dest, &mut budget);
            match expected {
                None => {
                    result.unwrap();
                    assert_eq!(std::fs::read(dest.join("first")).unwrap(), b"1234");
                    assert_eq!(std::fs::read(dest.join("nested/second")).unwrap(), b"5678");
                }
                Some(message) => assert!(format!("{:#}", result.unwrap_err()).contains(message)),
            }
            let written: u64 = ["first", "nested/second"]
                .iter()
                .filter_map(|name| std::fs::metadata(dest.join(name)).ok())
                .map(|metadata| metadata.len())
                .sum();
            assert!(written <= bytes);
        }
    }

    #[tokio::test]
    async fn remote_copy_bounds_file_that_grows_after_open() {
        use std::io::Write;

        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let entry = source.path().join("file");
        std::fs::write(&entry, b"1234").unwrap();
        let mut gate = test_gate::install(&entry, "copy-content");
        let root = source.path().to_owned();
        let tree = dest.path().join("tree");
        let output = tree.join("file");
        let task = tokio::task::spawn_blocking(move || {
            let anchored = crate::session::AnchoredDir::open(&root)?;
            let mut budget = CopyBudget {
                bytes: 4,
                entries: 1,
            };
            copy_anchored_tree(&anchored, &tree, &mut budget)
        });
        tokio::time::timeout(Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(entry)
            .unwrap()
            .write_all(b"5")
            .unwrap();
        gate.release();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("plugin staging budget exceeded"));
        assert_eq!(std::fs::read(output).unwrap(), b"1234");
    }

    #[tokio::test]
    async fn asset_download_bounds_streamed_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/asset", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/asset",
            axum::routing::get(|| async {
                axum::body::Body::from_stream(futures_util::stream::iter([
                    Ok::<_, std::io::Error>(vec![b'x'; 1024]),
                ]))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let work = tempfile::tempdir().unwrap();
        let complete = work.path().join("complete");
        download_asset(&url, &complete, 1024).await.unwrap();
        assert_eq!(std::fs::read(complete).unwrap(), vec![b'x'; 1024]);
        let limited = work.path().join("limited");
        let error = download_asset(&url, &limited, 512).await.unwrap_err();
        assert!(error.to_string().contains("plugin staging budget exceeded"));
        assert!(std::fs::metadata(limited).unwrap().len() <= 512);
        server.abort();
    }

    #[test]
    fn archive_limit_bounds_file_and_pax_contents() {
        let work = tempfile::tempdir().unwrap();
        for (name, kind) in [
            ("file", tar::EntryType::Regular),
            ("pax", tar::EntryType::XHeader),
        ] {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let data = vec![b'x'; 4096];
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, &data[..]).unwrap();
            let bytes = archive.into_inner().unwrap().finish().unwrap();
            let asset = work.path().join(format!("{name}.tgz"));
            std::fs::write(&asset, bytes).unwrap();
            let tree = work.path().join(name);
            std::fs::create_dir(&tree).unwrap();
            let error = extract_asset_archive(
                &tree,
                "fixture.tgz",
                std::fs::File::open(asset).unwrap(),
                None,
                1024,
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("plugin staging budget exceeded"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn archive_metadata_is_bounded_without_limiting_file_contents() {
        let work = tempfile::tempdir().unwrap();
        let data = vec![b'x'; ARCHIVE_METADATA_LIMIT as usize + 1024];
        for (name, kind) in [
            ("pax", tar::EntryType::XHeader),
            ("global-pax", tar::EntryType::XGlobalHeader),
            ("long-name", tar::EntryType::GNULongName),
            ("long-link", tar::EntryType::GNULongLink),
            ("file", tar::EntryType::Regular),
        ] {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, &data[..]).unwrap();
            let asset = work.path().join(format!("{name}.tgz"));
            std::fs::write(&asset, archive.into_inner().unwrap().finish().unwrap()).unwrap();
            let tree = work.path().join(name);
            std::fs::create_dir(&tree).unwrap();
            let result = extract_asset_archive(
                &tree,
                "fixture.tgz",
                std::fs::File::open(asset).unwrap(),
                None,
                2 * ARCHIVE_METADATA_LIMIT,
            );
            if kind == tar::EntryType::Regular {
                result.unwrap();
                assert_eq!(std::fs::read(tree.join(name)).unwrap(), data);
            } else {
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains("plugin archive metadata budget exceeded"),
                    "{error:#}"
                );
            }
        }

        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let chunk = &data[..(ARCHIVE_METADATA_LIMIT as usize * 3 / 4)];
        for kind in [tar::EntryType::XHeader, tar::EntryType::GNULongName] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(chunk.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, "metadata", chunk).unwrap();
        }
        let asset = work.path().join("aggregate.tgz");
        std::fs::write(&asset, archive.into_inner().unwrap().finish().unwrap()).unwrap();
        let tree = work.path().join("aggregate");
        std::fs::create_dir(&tree).unwrap();
        let error = extract_asset_archive(
            &tree,
            "aggregate.tgz",
            std::fs::File::open(asset).unwrap(),
            None,
            2 * ARCHIVE_METADATA_LIMIT,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("plugin archive metadata budget exceeded"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_asset_never_follows_links_or_replaces_the_source_snapshot() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let work = tempfile::tempdir().unwrap();
        let victim = work.path().join("victim");
        std::fs::write(&victim, b"private").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Archive symlink selected as `bin` must not be opened for chmod.
        let tree = work.path().join("symlink-tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("aoe-plugin.toml"), b"id = \"acme.bin\"\n").unwrap();
        // Absolute entry names cannot be produced through the safe builder, so
        // write the header directly; real archives can still carry them.
        let link_archive = work.path().join("link.tgz");
        {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.as_mut_bytes()[..11].copy_from_slice(b"/etc/passwd");
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(7);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append(&header, &b"ignored"[..]).unwrap();
            let mut encoder = archive.into_inner().unwrap();
            encoder.flush().unwrap();
            std::fs::write(&link_archive, encoder.finish().unwrap()).unwrap();
        }
        let error =
            install_asset_into(&tree, "link.tgz", Some("worker"), &link_archive).unwrap_err();
        assert!(
            format!("{error:#}").contains("must be a relative path inside the plugin"),
            "{error:#}"
        );

        let symlinked_archive = work.path().join("symlinked.tgz");
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        archive.append_link(&mut header, "worker", &victim).unwrap();
        let mut encoder = archive.into_inner().unwrap();
        encoder.flush().unwrap();
        std::fs::write(&symlinked_archive, encoder.finish().unwrap()).unwrap();
        let error = install_asset_into(&tree, "symlinked.tgz", Some("worker"), &symlinked_archive)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("unsupported entry"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"private");
        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A regular entry over an existing source file would change the
        // consented manifest, so the overlay is refused.
        let manifest_archive = work.path().join("manifest.tgz");
        std::fs::write(
            &manifest_archive,
            gzip(&[("aoe-plugin.toml", Some(b"id = \"evil\"\n"), 0o644)]),
        )
        .unwrap();
        let error = install_asset_into(
            &tree,
            "manifest.tgz",
            Some("aoe-plugin.toml"),
            &manifest_archive,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("would replace"), "{error:#}");
        assert_eq!(
            std::fs::read(tree.join("aoe-plugin.toml")).unwrap(),
            b"id = \"acme.bin\"\n"
        );

        // Raw assets may not replace a source file either.
        std::fs::write(tree.join("worker"), b"source").unwrap();
        let raw = work.path().join("worker");
        std::fs::write(&raw, b"new").unwrap();
        let error = install_asset_into(&tree, "worker", None, &raw).unwrap_err();
        assert!(format!("{error:#}").contains("would replace"), "{error:#}");
        assert_eq!(std::fs::read(tree.join("worker")).unwrap(), b"source");

        // Legitimate additions still land, with their archive mode.
        let additions = work.path().join("additions.tgz");
        std::fs::write(
            &additions,
            gzip(&[
                ("bin/", None, 0o755),
                ("bin/worker", Some(b"#!/bin/sh\nexit 0\n"), 0o750),
                ("empty/", None, 0o755),
            ]),
        )
        .unwrap();
        install_asset_into(&tree, "additions.tgz", Some("bin/worker"), &additions).unwrap();
        assert_eq!(
            std::fs::metadata(tree.join("bin/worker"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(tree.join("empty").is_dir());

        // Archives that list only files still create their parent directories.
        let nested = work.path().join("nested.tgz");
        std::fs::write(
            &nested,
            gzip(&[("deep/inner/worker", Some(b"#!/bin/sh\n"), 0o644)]),
        )
        .unwrap();
        install_asset_into(&tree, "nested.tgz", Some("deep/inner/worker"), &nested).unwrap();
        assert!(tree.join("deep/inner/worker").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_read_refuses_links_missing_and_oversized_files() {
        use std::os::unix::fs::symlink;
        let work = tempfile::tempdir().unwrap();
        let tree = work.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        assert!(read_manifest(&tree).is_err());

        let victim = work.path().join("victim");
        std::fs::write(&victim, b"id = \"acme.evil\"\n").unwrap();
        symlink(&victim, tree.join("aoe-plugin.toml")).unwrap();
        assert!(read_manifest(&tree).is_err());

        std::fs::remove_file(tree.join("aoe-plugin.toml")).unwrap();
        std::fs::write(
            tree.join("aoe-plugin.toml"),
            vec![b'#'; MAX_MANIFEST_BYTES + 1],
        )
        .unwrap();
        assert!(read_manifest(&tree).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn archive_asset_accepts_root_metadata_and_execute_only_bin() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let work = tempfile::tempdir().unwrap();
        let tree = work.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let asset = work.path().join("asset.tgz");
        {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let mut root = tar::Header::new_gnu();
            root.set_entry_type(tar::EntryType::Directory);
            root.set_size(0);
            root.set_mode(0o755);
            root.set_cksum();
            archive
                .append_data(&mut root, "./", std::io::empty())
                .unwrap();
            let mut global = tar::Header::new_gnu();
            let pax_name = b"././@PaxHeader";
            global.as_mut_bytes()[..pax_name.len()].copy_from_slice(pax_name);
            global.set_entry_type(tar::EntryType::XGlobalHeader);
            global.set_size(10);
            global.set_mode(0o644);
            global.set_cksum();
            archive.append(&global, &b"comment=x\n"[..]).unwrap();
            archive
                .append_pax_extensions([("path", &b"worker"[..])])
                .unwrap();
            let raw = b"#!/bin/sh\nexit 0\n";
            let mut worker = tar::Header::new_gnu();
            worker.set_entry_type(tar::EntryType::Regular);
            worker.set_size(raw.len() as u64);
            worker.set_mode(0o111);
            worker.set_cksum();
            archive
                .append_data(&mut worker, "worker", &raw[..])
                .unwrap();
            let mut encoder = archive.into_inner().unwrap();
            encoder.flush().unwrap();
            std::fs::write(&asset, encoder.finish().unwrap()).unwrap();
        }
        install_asset_into(&tree, "asset.tgz", Some("worker"), &asset).unwrap();
        assert_eq!(
            std::fs::read(tree.join("worker")).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
        assert_eq!(
            std::fs::metadata(tree.join("worker"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

#[cfg(test)]
pub(crate) mod test_gate {
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Mutex};

    struct Hook {
        path: PathBuf,
        phase: &'static str,
        entered: tokio::sync::oneshot::Sender<PathBuf>,
        release: mpsc::Receiver<bool>,
    }
    static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

    pub(crate) struct Gate {
        pub entered: tokio::sync::oneshot::Receiver<PathBuf>,
        release: Option<mpsc::Sender<bool>>,
        path: PathBuf,
        phase: &'static str,
    }
    impl Gate {
        pub(crate) async fn wait_for_preparation(&mut self) {
            // Disk-image acquisition has its own bounded backend deadline. Do
            // not charge it to the five-second behavior assertion that follows.
            tokio::time::timeout(std::time::Duration::from_secs(90), &mut self.entered)
                .await
                .expect("plugin staging preparation must reach its checkpoint")
                .expect("plugin staging preparation checkpoint must remain live");
            self.release();
        }

        pub(crate) fn release(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(false);
            }
        }

        #[cfg(unix)]
        pub(crate) fn panic_on_release(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(true);
            }
        }
    }
    impl Drop for Gate {
        fn drop(&mut self) {
            self.release();
            HOOKS
                .lock()
                .unwrap()
                .retain(|hook| hook.path != self.path || hook.phase != self.phase);
        }
    }
    pub(crate) fn install(path: &Path, phase: &'static str) -> Gate {
        let path = scope_path(path);
        let (entered, receiver) = tokio::sync::oneshot::channel();
        let (release, waiting) = mpsc::channel();
        HOOKS.lock().unwrap().push(Hook {
            path: path.clone(),
            phase,
            entered,
            release: waiting,
        });
        Gate {
            entered: receiver,
            release: Some(release),
            path,
            phase,
        }
    }
    pub(crate) fn reach(path: &Path, phase: &'static str) {
        reach_observed(path, path, phase);
    }

    pub(crate) fn reach_observed(scope: &Path, path: &Path, phase: &'static str) {
        let scope = scope_path(scope);
        let hook = {
            let mut hooks = HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.phase == phase && scope.starts_with(&hook.path))
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(path.to_owned());
            if hook
                .release
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("test must release plugin worker gate")
            {
                panic!("private-panic-payload");
            }
        }
    }

    fn scope_path(path: &Path) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            // /var and /private/var name the same directory on macOS. A gate
            // may precede publication, so resolve the existing ancestor only.
            for ancestor in path.ancestors() {
                if let Ok(root) = ancestor.canonicalize() {
                    return root.join(path.strip_prefix(ancestor).expect("path ancestor"));
                }
            }
        }
        // Linux staging roots can be /proc/PID/root paths into another mount
        // namespace. Never resolve those in the observing process.
        path.to_owned()
    }

    #[tokio::test]
    async fn gate_matches_existing_ancestor_and_preserves_observed_path() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("not-published");
        let mut gate = install(&scope, "scope-alias");
        #[cfg(target_os = "macos")]
        let reached_scope = root.path().canonicalize().unwrap().join("not-published");
        #[cfg(not(target_os = "macos"))]
        let reached_scope = scope;
        let observed = PathBuf::from("/proc/123/root/tree");
        let worker_observation = observed.clone();
        let task = tokio::task::spawn_blocking(move || {
            reach_observed(&reached_scope, &worker_observation, "scope-alias");
        });
        let entered = tokio::time::timeout(std::time::Duration::from_secs(5), &mut gate.entered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entered, observed);
        gate.release();
        task.await.unwrap();
    }
}
