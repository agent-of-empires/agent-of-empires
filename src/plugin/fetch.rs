//! Fetching an external plugin into a staging tree, ready to be moved into
//! place by [`crate::plugin::install`].
//!
//! Two source kinds, selected by [`PluginSource`]:
//!
//! - A GitHub repo is `git clone`d (shallow when possible), the requested ref
//!   is checked out, and the exact commit is resolved for the lockfile. The
//!   `.git` directory is stripped; the working tree is the plugin.
//! - A local directory is copied verbatim (minus `.git`).
//!
//! If the manifest declares a `release-binary` runtime, the matching release
//! asset for the host platform is downloaded from the repo's GitHub releases
//! and unpacked into the tree. The worker is not launched here; that is #2095.
//! A local source never fetches a release: its binary must already be present.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{PluginManifest, RuntimeSpec};

use crate::github::{GitHubClient, GitHubClientConfig, GitHubError, DEFAULT_USER_AGENT};

use super::source::PluginSource;

/// A plugin fetched into a staging tree, not yet installed.
pub struct FetchedPlugin {
    /// Keeps the staging directory alive until the tree is moved into place.
    _staging: tempfile::TempDir,
    /// The plugin tree to move to `<app_dir>/plugins/<id>/`.
    pub tree: PathBuf,
    pub manifest: PluginManifest,
    /// Raw `aoe-plugin.toml` bytes, for hashing the grant against.
    pub manifest_bytes: Vec<u8>,
    /// `sha256:<hex>` over the source tree, computed before any release-binary
    /// is injected so it matches an author's `aoe plugin hash` of the checkout.
    pub tree_hash: String,
    pub source: PluginSource,
    pub requested_ref: Option<String>,
    pub resolved_commit: Option<String>,
    pub release_tag: Option<String>,
    pub asset_name: Option<String>,
    pub asset_sha256: Option<String>,
}

/// Fetch a plugin from its source into a staging tree.
pub async fn fetch(source: &PluginSource) -> Result<FetchedPlugin> {
    let plugins_root = super::plugins_dir()?;
    std::fs::create_dir_all(&plugins_root)
        .with_context(|| format!("creating {}", plugins_root.display()))?;
    // Stage under the plugins dir so the final rename into place is same-filesystem.
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&plugins_root)
        .context("creating plugin staging dir")?;
    let tree = staging.path().join("tree");

    let (requested_ref, resolved_commit) = match source {
        PluginSource::Github { reference, .. } => {
            let url = source
                .github_clone_url()
                .expect("github source yields a clone url");
            let reference = reference.clone();
            let tree_clone = tree.clone();
            let sha = tokio::task::spawn_blocking(move || {
                git_clone_checkout(&url, reference.as_deref(), &tree_clone)
            })
            .await
            .context("git clone task panicked")??;
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

    // The reserved build-output dir is excluded from the tree hash, so a source
    // that ships it could hide files from the pin. It must only ever be created
    // by build steps, never committed; refuse a source tree that contains it.
    if tree.join(super::integrity::BUILD_OUTPUT_DIR).exists() {
        bail!(
            "plugin source ships the reserved build-output directory {:?}; it must only be produced by build steps, not committed",
            super::integrity::BUILD_OUTPUT_DIR
        );
    }

    // Hash the source tree before any release-binary is injected below, so the
    // value matches `aoe plugin hash` run on the author's checkout (which has
    // no downloaded worker) and can be checked against the featured pin.
    let tree_hash = super::integrity::tree_hash(&tree)?;

    let mut release_tag = None;
    let mut asset_name = None;
    let mut asset_sha256 = None;
    if let Some(RuntimeSpec::ReleaseBinary { asset, bin }) = &manifest.runtime {
        match source {
            PluginSource::Github { .. } => {
                let (tag, name, sha) = download_release_binary(
                    source,
                    &manifest,
                    asset,
                    bin.as_deref(),
                    &tree,
                    requested_ref.as_deref(),
                )
                .await?;
                release_tag = Some(tag);
                asset_name = Some(name);
                asset_sha256 = Some(sha);
            }
            PluginSource::Local(_) => {
                // A local source ships its binary in the directory already; there
                // is no release to pull from.
            }
        }
    }

    Ok(FetchedPlugin {
        _staging: staging,
        tree,
        manifest,
        manifest_bytes,
        tree_hash,
        source: source.clone(),
        requested_ref,
        resolved_commit,
        release_tag,
        asset_name,
        asset_sha256,
    })
}

fn read_manifest(tree: &Path) -> Result<(PluginManifest, Vec<u8>)> {
    let path = tree.join("aoe-plugin.toml");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("no aoe-plugin.toml at {}", path.display()))?;
    let text = std::str::from_utf8(&bytes).context("aoe-plugin.toml is not valid UTF-8")?;
    let manifest = PluginManifest::from_toml_str(text).map_err(|e| anyhow!("{e}"))?;
    Ok((manifest, bytes))
}

/// Run `git` with the given args, returning trimmed stdout. Surfaces stderr on
/// failure and a clear hint when git is not installed.
// ponytail: no explicit timeout; git fails on its own for unreachable remotes.
fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd
        .output()
        .map_err(|e| anyhow!("failed to run git (is it installed and on PATH?): {e}"))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Resolve the commit a GitHub source's ref currently points at, without
/// cloning, via `git ls-remote`. `reference` of `None` means the remote `HEAD`.
/// Used by the update check to tell whether a newer commit exists.
///
/// A `reference` that is already a full commit sha is returned as-is: `ls-remote`
/// cannot resolve a bare sha, and a commit-pinned install can never be outdated.
/// For a branch or tag, an annotated tag's peeled (`^{}`) target is preferred so
/// the result is the commit the install would actually check out.
pub fn ls_remote(url: &str, reference: Option<&str>) -> Result<String> {
    if let Some(r) = reference {
        if is_full_commit_sha(r) {
            return Ok(r.to_ascii_lowercase());
        }
    }
    let target = reference.unwrap_or("HEAD");
    let out = run_git(&["ls-remote", url, target], None)?;
    parse_ls_remote(&out, target)
}

/// The tag of the repo's latest stable GitHub release, or `None` when the repo
/// has published no release. `GET /releases/latest` already excludes prereleases
/// and drafts, so this is the stable channel; a 404 (no releases) maps to `None`
/// while any other API error propagates. Used by the default install path
/// (no `@ref`) and the rolling update check.
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

/// Pick the resolved commit from `git ls-remote` output (`<sha>\t<ref>` lines),
/// preferring an annotated tag's peeled `^{}` target over the tag object itself.
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

/// Clone `url` into `dest`, check out `reference` (if any), strip `.git`, and
/// return the resolved commit. A shallow clone of the ref is tried first; an
/// arbitrary commit ref falls back to a full clone plus checkout.
fn git_clone_checkout(url: &str, reference: Option<&str>, dest: &Path) -> Result<String> {
    let dest_str = path_arg(dest)?;

    // `core.autocrlf=false` keeps the checkout byte-for-byte as committed, so
    // the tree hash is the same on every platform; without it a Windows clone
    // would rewrite line endings and never match a pin generated on Linux.
    let shallow = match reference {
        Some(reference) => run_git(
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
        )
        .is_ok(),
        None => run_git(
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
        )
        .is_ok(),
    };

    if !shallow {
        // A partial clone may have created dest; clear it before retrying.
        let _ = std::fs::remove_dir_all(dest);
        run_git(
            &["-c", "core.autocrlf=false", "clone", "--", url, dest_str],
            None,
        )?;
        if let Some(reference) = reference {
            // `--` separates the revision from pathspecs so a ref that begins
            // with a dash is not parsed as a flag.
            run_git(
                &[
                    "-c",
                    "advice.detachedHead=false",
                    "checkout",
                    reference,
                    "--",
                ],
                Some(dest),
            )?;
        }
    }

    let sha = run_git(&["rev-parse", "HEAD"], Some(dest))?;
    // The plugin is the working tree, not a git checkout; drop the history.
    let _ = std::fs::remove_dir_all(dest.join(".git"));
    Ok(sha)
}

/// Recursively copy `src` into `dst`, skipping `.git` and rejecting symlinks.
///
/// A symlink is a hard error rather than a silent skip: `integrity::tree_hash`
/// also rejects symlinks, so skipping one here would make the install-time hash
/// disagree with the `aoe plugin hash` an author runs on the same directory
/// (and following one risks escaping the tree).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(&name);
        if file_type.is_symlink() {
            bail!(
                "plugin source contains a symlink ({}); symlinks are not allowed",
                from.display()
            );
        } else if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn github_api_base() -> String {
    std::env::var("AOE_UPDATE_API_BASE")
        .unwrap_or_else(|_| crate::github::DEFAULT_GITHUB_API_BASE.to_string())
}

/// Resolve the release for the host platform, download the matching asset, and
/// unpack it into `tree`. Returns `(release_tag, asset_name, asset_sha256)`.
async fn download_release_binary(
    source: &PluginSource,
    manifest: &PluginManifest,
    asset_template: &str,
    bin: Option<&str>,
    tree: &Path,
    requested_ref: Option<&str>,
) -> Result<(String, String, String)> {
    let (owner, repo) = match source {
        PluginSource::Github { owner, repo, .. } => (owner.as_str(), repo.as_str()),
        PluginSource::Local(_) => bail!("a release-binary worker requires a GitHub source"),
    };

    let client = GitHubClient::unauthenticated(GitHubClientConfig {
        api_base: github_api_base(),
        user_agent: DEFAULT_USER_AGENT.to_string(),
        timeout: Duration::from_secs(60),
    })?;

    let release = match requested_ref {
        Some(tag) => client
            .release_by_tag(owner, repo, tag)
            .await
            .with_context(|| format!("no release tagged {tag:?} for {owner}/{repo}"))?,
        None => client
            .latest_release(owner, repo)
            .await
            .with_context(|| format!("no latest release for {owner}/{repo}"))?,
    };

    let wanted = render_asset_template(asset_template, &manifest.version);
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

    let bytes = http_get_bytes(&asset.browser_download_url).await?;
    let sha = sha256_hex(&bytes);
    install_asset_into(tree, &asset.name, bin, &bytes)?;
    Ok((release.tag_name.clone(), asset.name.clone(), sha))
}

/// Substitute the platform tokens in an asset name template. Supported:
/// `${os}` (e.g. `linux`, `macos`), `${arch}` (e.g. `x86_64`, `aarch64`), and
/// `${version}` (the manifest version).
fn render_asset_template(template: &str, version: &str) -> String {
    template
        .replace("${os}", std::env::consts::OS)
        .replace("${arch}", std::env::consts::ARCH)
        .replace("${version}", version)
}

async fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(Duration::from_secs(300))
        .build()?;
    let response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} failed: HTTP {status}");
    }
    Ok(response.bytes().await?.to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Place a downloaded asset into the plugin tree. A `.tar.gz` archive is
/// unpacked and `bin` (required) names the executable within; any other asset
/// is treated as a raw binary written as `bin` (or the asset name). The result
/// is made executable.
fn install_asset_into(
    tree: &Path,
    asset_name: &str,
    bin: Option<&str>,
    bytes: &[u8],
) -> Result<()> {
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(tree)
            .with_context(|| format!("unpacking {asset_name}"))?;
        let bin_rel =
            bin.ok_or_else(|| anyhow!("a release-binary archive asset must set `bin`"))?;
        ensure_executable(&safe_tree_path(tree, bin_rel)?)
    } else {
        let name = bin.unwrap_or(asset_name);
        let path = safe_tree_path(tree, name)?;
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        ensure_executable(&path)
    }
}

/// Join a manifest-provided relative path onto the plugin tree, rejecting
/// anything that would escape it. `bin` is untrusted manifest input, so an
/// absolute path or a `..` component must not turn install into an arbitrary
/// write or chmod outside the staging dir.
fn safe_tree_path(tree: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let candidate = Path::new(rel);
    let safe = candidate
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !safe || candidate.as_os_str().is_empty() {
        bail!("plugin path {rel:?} must be a relative path inside the plugin (no absolute path or `..`)");
    }
    Ok(tree.join(candidate))
}

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        bail!(
            "expected binary {} missing after extraction",
            path.display()
        );
    }
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("making {} executable", path.display()))
}

#[cfg(not(unix))]
fn ensure_executable(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!(
            "expected binary {} missing after extraction",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ls_remote_prefers_peeled_tag() {
        let out = "1111111111111111111111111111111111111111\trefs/tags/v1\n\
                   2222222222222222222222222222222222222222\trefs/tags/v1^{}";
        assert_eq!(
            parse_ls_remote(out, "v1").unwrap(),
            "2222222222222222222222222222222222222222"
        );
    }

    #[test]
    fn parse_ls_remote_takes_first_when_unpeeled() {
        let out = "3333333333333333333333333333333333333333\tHEAD";
        assert_eq!(
            parse_ls_remote(out, "HEAD").unwrap(),
            "3333333333333333333333333333333333333333"
        );
    }

    #[test]
    fn parse_ls_remote_errors_when_empty() {
        assert!(parse_ls_remote("", "nope").is_err());
    }

    #[test]
    fn ls_remote_returns_pinned_sha_as_is() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        assert_eq!(ls_remote("unused://", Some(sha)).unwrap(), sha);
    }

    #[test]
    fn renders_platform_tokens() {
        let rendered = render_asset_template("w-${os}-${arch}-${version}.tar.gz", "1.2.3");
        assert!(rendered.starts_with("w-"));
        assert!(rendered.ends_with("-1.2.3.tar.gz"));
        assert!(rendered.contains(std::env::consts::OS));
        assert!(rendered.contains(std::env::consts::ARCH));
    }

    #[test]
    fn safe_tree_path_rejects_escapes() {
        let tree = Path::new("/plugins/acme");
        assert!(safe_tree_path(tree, "bin/worker").is_ok());
        assert!(safe_tree_path(tree, "worker").is_ok());
        for bad in ["../../.bashrc", "/etc/passwd", "a/../../b", ""] {
            assert!(
                safe_tree_path(tree, bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn raw_asset_is_written_executable() {
        let dir = tempfile::tempdir().unwrap();
        install_asset_into(dir.path(), "thing", Some("thing"), b"#!/bin/sh\n").unwrap();
        let path = dir.path().join("thing");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "should be executable, mode {mode:o}");
        }
    }

    #[test]
    fn copy_tree_skips_git() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("aoe-plugin.toml"), b"x").unwrap();
        std::fs::create_dir(src.path().join(".git")).unwrap();
        std::fs::write(src.path().join(".git").join("config"), b"y").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let into = dst.path().join("tree");
        copy_tree(src.path(), &into).unwrap();
        assert!(into.join("aoe-plugin.toml").exists());
        assert!(!into.join(".git").exists());
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_rejects_symlinks() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", src.path().join("link")).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let err = copy_tree(src.path(), &dst.path().join("tree"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "got: {err}");
    }
}
