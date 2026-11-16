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
    staging: Option<tempfile::TempDir>,
    operation: Option<Arc<crate::session::StorageFlock>>,
    pub tree: PathBuf,
    pub manifest: PluginManifest,
    pub manifest_bytes: Vec<u8>,
    pub tree_hash: String,
    pub source: PluginSource,
    pub requested_ref: Option<String>,
    pub resolved_commit: Option<String>,
    pub release_tag: Option<String>,
    pub asset_name: Option<String>,
    pub asset_sha256: Option<String>,
    pub(super) budget: super::execution::Budget,
}

impl FetchedPlugin {
    pub(super) fn close(mut self) {
        drop(self.staging.take());
    }

    pub(super) async fn cleanup(self) -> Result<()> {
        tokio::task::spawn_blocking(move || self.close())
            .await
            .context("plugin staging cleanup task failed")
    }
}

impl Drop for FetchedPlugin {
    fn drop(&mut self) {
        if let Some(staging) = self.staging.take() {
            // Cancellation or validation failure must not remove a large tree on a runtime worker.
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    let operation = self.operation.take();
                    drop(runtime.spawn_blocking(move || {
                        drop(staging);
                        drop(operation);
                    }));
                }
                Err(_) => drop(staging),
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
        download_release_binary(fetched, operation).await
    } else {
        Ok(fetched)
    }
}

fn stage(
    source: PluginSource,
    plugins_root: PathBuf,
    clone_url: Option<String>,
) -> Result<FetchedPlugin> {
    let budget = super::execution::Budget::new();
    std::fs::create_dir_all(&plugins_root)
        .with_context(|| format!("creating {}", plugins_root.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&plugins_root)
        .context("creating plugin staging dir")?;
    let tree = staging.path().join("tree");

    let result = super::execution::catch_panic(|| {
        let (requested_ref, resolved_commit) = match &source {
            PluginSource::Github { reference, .. } => {
                let sha = git_clone_checkout(
                    clone_url
                        .as_deref()
                        .expect("github source yields a clone url"),
                    reference.as_deref(),
                    &tree,
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
        Ok(FetchedPlugin {
            staging: None,
            operation: None,
            tree,
            manifest,
            manifest_bytes,
            tree_hash,
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
            if error
                .downcast_ref::<super::execution::RecoveryRequired>()
                .is_some()
            {
                let retained = staging.keep();
                tracing::error!(target: "plugin.install", path = %retained.display(), %error, "plugin staging retained for recovery");
            }
            Err(error)
        }
    }
}

/// A manifest is small; the cap keeps a hostile symlink or huge file from
/// being read before the tree hash rejects it.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

fn read_manifest(tree: &Path) -> Result<(PluginManifest, Vec<u8>)> {
    let root = crate::session::AnchoredDir::open(tree)?;
    let Some(mut file) = root.open_regular(Path::new("aoe-plugin.toml"), MAX_MANIFEST_BYTES)?
    else {
        bail!(
            "no regular aoe-plugin.toml of at most {MAX_MANIFEST_BYTES} bytes at {}; \
             this repository is not an installable plugin. \
             The GitHub `aoe-plugin` topic is also used by unrelated Age of Empires projects.",
            tree.join("aoe-plugin.toml").display()
        );
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", tree.join("aoe-plugin.toml").display()))?;
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

fn git_command(args: &[&str], cwd: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd
}

fn git_failure(args: &[&str], output: &std::process::Output) -> anyhow::Error {
    GitFailed {
        command: args.join(" "),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
    .into()
}

fn run_git(args: &[&str], cwd: Option<&Path>, budget: super::execution::Budget) -> Result<String> {
    let mut cmd = git_command(args, cwd);
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
    cwd: Option<&Path>,
    budget: super::execution::Budget,
) -> Result<()> {
    let mut cmd = git_command(args, cwd);
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
    let target = reference.unwrap_or("HEAD");
    let peeled = format!("{target}^{{}}");
    let out = run_git(
        &["ls-remote", url, target, &peeled],
        None,
        super::execution::Budget::new(),
    )?;
    parse_ls_remote(&out, target)
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

/// Prefers an annotated tag's peeled `^{}` commit, which is what the install checks out (#2646).
fn parse_ls_remote(out: &str, target: &str) -> Result<String> {
    let mut first = None;
    for line in out.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if name.ends_with("^{}") {
            return Ok(sha.trim().to_string());
        }
        first.get_or_insert_with(|| sha.trim().to_string());
    }
    first.ok_or_else(|| anyhow!("ref {target:?} not found on the remote"))
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", path.display()))
}

/// `core.autocrlf=false` keeps the checkout byte-identical so tree hashes match across platforms.
fn git_clone_checkout(
    url: &str,
    reference: Option<&str>,
    dest: &Path,
    budget: super::execution::Budget,
) -> Result<String> {
    let dest_str = path_arg(dest)?;

    let shallow = match reference {
        Some(reference) => run_git_discarding_stdout(
            &[
                "-c",
                "core.autocrlf=false",
                "clone",
                "--depth",
                "1",
                "--branch",
                reference,
                "--",
                url,
                dest_str,
            ],
            None,
            budget,
        ),
        None => run_git_discarding_stdout(
            &[
                "-c",
                "core.autocrlf=false",
                "clone",
                "--depth",
                "1",
                "--",
                url,
                dest_str,
            ],
            None,
            budget,
        ),
    };

    if let Err(error) = shallow {
        if error.downcast_ref::<GitFailed>().is_none() {
            return Err(error);
        }
        let _ = std::fs::remove_dir_all(dest);
        run_git_discarding_stdout(
            &["-c", "core.autocrlf=false", "clone", "--", url, dest_str],
            None,
            budget,
        )?;
        if let Some(reference) = reference {
            run_git_discarding_stdout(
                &[
                    "-c",
                    "advice.detachedHead=false",
                    "checkout",
                    reference,
                    "--",
                ],
                Some(dest),
                budget,
            )?;
        }
    }

    let sha = run_git(&["rev-parse", "HEAD"], Some(dest), budget)?;
    let _ = std::fs::remove_dir_all(dest.join(".git"));
    Ok(sha)
}

/// Symlinks are an error, not skipped, so the install hash agrees with `integrity::tree_hash`.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    // Resolve only the root spelling. Descendants must never follow symlinks.
    let root = std::fs::canonicalize(src)
        .with_context(|| format!("resolving plugin source {}", src.display()))?;
    let source = crate::session::AnchoredDir::open(&root)?;
    copy_anchored_tree(&source, dst)
}

fn copy_anchored_tree(source: &crate::session::AnchoredDir, dst: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for name in source.read_dir_complete(Path::new(""), usize::MAX)? {
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
                copy_anchored_tree(&child, &to)?;
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
                let mut output = std::fs::File::create(&to)?;
                std::io::copy(&mut input, &mut output)
                    .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
                output.set_permissions(metadata.permissions())?;
            }
            _ => bail!(
                "plugin source contains an unsupported special file ({})",
                from.display()
            ),
        }
    }
    Ok(())
}

pub(super) fn github_api_base() -> String {
    std::env::var("AOE_UPDATE_API_BASE")
        .unwrap_or_else(|_| crate::github::DEFAULT_GITHUB_API_BASE.to_string())
}

async fn download_release_binary(
    fetched: FetchedPlugin,
    operation: Option<Arc<crate::session::StorageFlock>>,
) -> Result<FetchedPlugin> {
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
        download_asset(&asset.browser_download_url, &asset_path).await?;
        Ok::<_, anyhow::Error>((
            asset_path.to_path_buf(),
            asset.name.clone(),
            release.tag_name,
            bin.clone(),
        ))
    }
    .await;
    let (asset_path, name, tag, bin) = match download {
        Ok(download) => download,
        Err(error) => {
            fetched.cleanup().await?;
            return Err(error);
        }
    };
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let mut fetched = fetched;
        let result = super::execution::catch_panic(|| {
            #[cfg(test)]
            test_gate::reach(&fetched.tree, "asset");
            let sha = sha256_file(&asset_path)?;
            install_asset_into(&fetched.tree, &name, bin.as_deref(), &asset_path)?;
            fetched.release_tag = Some(tag);
            fetched.asset_name = Some(name);
            fetched.asset_sha256 = Some(sha);
            Ok(())
        });
        match result {
            Ok(()) => Ok(fetched),
            Err(error) => {
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
                    fetched.close();
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

async fn download_asset(url: &str, dest: &Path) -> Result<()> {
    use std::io::Write;
    let client = reqwest::Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(Duration::from_secs(300))
        .build()?;
    let mut response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} failed: HTTP {status}");
    }
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)
            .with_context(|| format!("writing {url} to {}", dest.display()))?;
    }
    file.flush()
        .with_context(|| format!("writing {url} to {}", dest.display()))
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
        extract_asset_archive(tree, asset_name, source, Some(&bin_rel))?;
        ensure_executable(tree, &bin_rel)
    } else {
        let name = checked_asset_path(Path::new(bin.unwrap_or(asset_name)))?;
        let root = crate::session::AnchoredDir::open(tree)?;
        if let Some(parent) = name.parent().filter(|p| !p.as_os_str().is_empty()) {
            root.ensure_dir(parent)?;
        }
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

/// Assets may only add files: a replacement would change the manifest or tree
/// after the consent fingerprint was computed over the source snapshot.
fn extract_asset_archive(
    tree: &Path,
    asset_name: &str,
    source: std::fs::File,
    executable: Option<&Path>,
) -> Result<()> {
    let root = crate::session::AnchoredDir::open(tree)?;
    let decoder = flate2::read::GzDecoder::new(source);
    for entry in tar::Archive::new(decoder)
        .entries()
        .with_context(|| format!("unpacking {asset_name}"))?
    {
        let mut entry = entry.with_context(|| format!("unpacking {asset_name}"))?;
        let kind = entry.header().entry_type();
        // A global PAX header describes later entries and names no path.
        if kind == tar::EntryType::XGlobalHeader {
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
                continue;
            }
            bail!("plugin path {entry_path:?} is empty");
        }
        let rel = checked_asset_path(&normalized)?;
        if kind.is_dir() {
            root.ensure_dir(&rel)
                .with_context(|| format!("unpacking {asset_name}"))?;
        } else if kind.is_file() {
            if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                root.ensure_dir(parent)
                    .with_context(|| format!("unpacking {asset_name}"))?;
            }
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

    #[test]
    fn parse_ls_remote_prefers_the_peeled_tag_and_a_pinned_sha_skips_the_remote() {
        let peeled = "1111111111111111111111111111111111111111\trefs/tags/v1\n\
                      2222222222222222222222222222222222222222\trefs/tags/v1^{}";
        assert_eq!(
            parse_ls_remote(peeled, "v1").unwrap(),
            "2222222222222222222222222222222222222222"
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
    fn ls_remote_peels_annotated_tag_to_commit() {
        use crate::git::test_support::run_git;

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
                async move { download.lock().unwrap().clone() }
            }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _env = EnvGuard::set(&[
            ("AOE_UPDATE_API_BASE", base.as_str()),
            (
                "AOE_GITHUB_CLONE_BASE",
                repositories.path().to_str().unwrap(),
            ),
        ]);
        let source = PluginSource::parse("gh:acme/bin@v1").unwrap();
        let plugins = crate::plugin::plugins_dir().unwrap();
        let mut gate = test_gate::install(&plugins, "asset");
        let operation = crate::plugin::install::begin_operation().unwrap();
        let staged_source = source.clone();
        let task = tokio::spawn(async move { fetch(&staged_source, Some(operation)).await });
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
        let before_injection =
            stage(source.clone(), plugins.clone(), source.github_clone_url()).unwrap();
        assert_eq!(fetched.tree_hash, before_injection.tree_hash);
        before_injection.cleanup().await.unwrap();
        fetched.cleanup().await.unwrap();
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
    async fn git_budget_exhaustion_never_falls_back_but_checkout_failure_does() {
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
        let calls = fixture.path().join("calls");
        let ready = fixture.path().join("ready");
        std::fs::write(
            &git,
            format!(
                "#!/bin/sh\nprintf 'call\\n' >> '{}'\n: > '{}'\nwhile :; do sleep 0.01; done\n",
                calls.display(),
                ready.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _path = crate::session::test_support::path_prepended(fixture.path());
        let budget = super::super::execution::Budget::controlled_for_test(&EXPIRED);
        let destination = fixture.path().join("checkout");
        let task = tokio::task::spawn_blocking(move || {
            git_clone_checkout("fixture", Some("v1"), &destination, budget)
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        EXPIRED.store(true, Ordering::SeqCst);
        let error = task.await.unwrap().unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::execution::Exhausted>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        std::fs::write(&calls, "").unwrap();
        std::fs::write(&git, format!(
            "#!/bin/sh\nprintf 'call\\n' >> '{}'\nprintf ordinary-checkout-failure >&2\nexit 1\n", calls.display()
        )).unwrap();
        EXPIRED.store(false, Ordering::SeqCst);
        let error =
            git_clone_checkout("fixture", Some("v1"), &fixture.path().join("retry"), budget)
                .unwrap_err();
        assert!(error.to_string().contains("ordinary-checkout-failure"));
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
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
        let (entered, receiver) = tokio::sync::oneshot::channel();
        let (release, waiting) = mpsc::channel();
        HOOKS.lock().unwrap().push(Hook {
            path: path.to_owned(),
            phase,
            entered,
            release: waiting,
        });
        Gate {
            entered: receiver,
            release: Some(release),
            path: path.to_owned(),
            phase,
        }
    }
    pub(crate) fn reach(path: &Path, phase: &'static str) {
        let hook = {
            let mut hooks = HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.phase == phase && path.starts_with(&hook.path))
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
}
