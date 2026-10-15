//! Per-surface repo trust store (`<app_dir>/trusted_repos.toml`) for hooks and project MCP.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::{load_repo_config, normalize_path, repo_config_source_path, HooksConfig};
use crate::session::mcp::project_mcp::ProjectMcpServer;

/// Hook and MCP trust are recorded independently so approving one never
/// re-authorizes a stale version of the other. Legacy rows have only `hooks_hash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustedRepo {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_hash: Option<String>,
    trusted_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrustedRepos {
    #[serde(default)]
    repos: Vec<TrustedRepo>,
}

pub fn compute_hooks_hash(hooks: &HooksConfig) -> String {
    let mut hasher = Sha256::new();
    for (label, cmds) in [
        (&b"on_create:"[..], &hooks.on_create),
        (b"on_launch:", &hooks.on_launch),
        (b"on_destroy:", &hooks.on_destroy),
    ] {
        for cmd in cmds {
            hasher.update(label);
            hasher.update(cmd.as_bytes());
            hasher.update(b"\n");
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

/// Shared across profiles, so a repo trusted in one needs no re-approval in another.
fn trusted_repos_path() -> Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join("trusted_repos.toml"))
}

fn load_trusted_repos() -> Result<TrustedRepos> {
    let path = trusted_repos_path()?;
    if !path.exists() {
        return Ok(TrustedRepos::default());
    }
    let content = std::fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(TrustedRepos::default());
    }
    Ok(toml::from_str(&content)?)
}

/// `Some(hash)` requires that surface to match; `None` ignores it. A repo with no row is never trusted.
pub fn is_repo_trusted(
    project_path: &Path,
    hooks_hash: Option<&str>,
    mcp_hash: Option<&str>,
) -> Result<bool> {
    let normalized = normalize_path(project_path);
    let matches = |stored: Option<&str>, want: Option<&str>| want.is_none() || stored == want;
    Ok(load_trusted_repos()?.repos.iter().any(|r| {
        r.path == normalized
            && matches(r.hooks_hash.as_deref(), hooks_hash)
            && matches(r.mcp_hash.as_deref(), mcp_hash)
    }))
}

/// Records trust per surface under the store lock; `None` keeps that surface's stored hash.
pub fn trust_repo(
    project_path: &Path,
    hooks_hash: Option<&str>,
    mcp_hash: Option<&str>,
) -> Result<()> {
    let normalized = normalize_path(project_path);
    crate::session::storage::locked_update(
        &trusted_repos_path()?,
        |content| toml::from_str(content).context("Failed to parse trusted_repos.toml"),
        |trusted: &TrustedRepos| Ok(toml::to_string_pretty(trusted)?),
        |trusted| {
            let existing = trusted.repos.iter().find(|r| r.path == normalized);
            let hooks_final = hooks_hash
                .map(str::to_string)
                .or_else(|| existing.and_then(|e| e.hooks_hash.clone()));
            let mcp_final = mcp_hash
                .map(str::to_string)
                .or_else(|| existing.and_then(|e| e.mcp_hash.clone()));
            trusted.repos.retain(|r| r.path != normalized);
            trusted.repos.push(TrustedRepo {
                path: normalized,
                hooks_hash: hooks_final,
                mcp_hash: mcp_final,
                trusted_at: chrono::Utc::now().to_rfc3339(),
            });
            Ok::<_, anyhow::Error>(())
        },
    )?
}

pub enum TrustSurface<T> {
    Absent,
    Trusted(T),
    /// Present but unapproved at its current fingerprint.
    NeedsTrust {
        config: T,
        hash: String,
    },
}

impl<T> TrustSurface<T> {
    pub fn needs_trust(&self) -> bool {
        matches!(self, TrustSurface::NeedsTrust { .. })
    }

    pub fn trusted(self) -> Option<T> {
        match self {
            TrustSurface::Trusted(config) => Some(config),
            _ => None,
        }
    }

    fn resolve(config: T, hash: String, stored: Option<&str>) -> Self {
        if stored == Some(hash.as_str()) {
            TrustSurface::Trusted(config)
        } else {
            TrustSurface::NeedsTrust { config, hash }
        }
    }
}

/// Both surfaces are read from the main repo, so what the dialog shows is what
/// later forwards; each resolves independently.
pub struct RepoTrust {
    pub project_path: String,
    pub hooks: TrustSurface<HooksConfig>,
    pub mcp: TrustSurface<Vec<ProjectMcpServer>>,
}

impl RepoTrust {
    pub fn needs_prompt(&self) -> bool {
        self.hooks.needs_trust() || self.mcp.needs_trust()
    }
}

pub fn check_repo_trust(project_path: &Path) -> Result<RepoTrust> {
    let normalized = normalize_path(&repo_config_source_path(project_path));
    let trusted = load_trusted_repos()?;
    let row = trusted.repos.iter().find(|r| r.path == normalized);

    let hooks = match load_repo_config(Path::new(&normalized))?.and_then(|rc| rc.hooks()) {
        Some(h) if !h.is_empty() => {
            let hash = compute_hooks_hash(&h);
            TrustSurface::resolve(h, hash, row.and_then(|r| r.hooks_hash.as_deref()))
        }
        _ => TrustSurface::Absent,
    };

    // A broken `.mcp.json` must not suppress hook trust; the supervisor is the real MCP gate.
    let servers =
        crate::session::mcp::project_mcp::load_project_mcp_servers(Path::new(&normalized))
            .unwrap_or_else(|e| {
                tracing::warn!(
                    target: "session.store",
                    path = %normalized,
                    error = %e,
                    "failed to load project .mcp.json for trust; treating as absent"
                );
                Vec::new()
            });
    let mcp = if servers.is_empty() {
        TrustSurface::Absent
    } else {
        let hash = crate::session::mcp::project_mcp::fingerprint(&servers);
        TrustSurface::resolve(servers, hash, row.and_then(|r| r.mcp_hash.as_deref()))
    };

    Ok(RepoTrust {
        project_path: normalized,
        hooks,
        mcp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_hash_covers_every_type_and_command() {
        let hooks = |create: &[&str], launch: &[&str], destroy: &[&str]| HooksConfig {
            on_create: create.iter().map(|s| s.to_string()).collect(),
            on_launch: launch.iter().map(|s| s.to_string()).collect(),
            on_destroy: destroy.iter().map(|s| s.to_string()).collect(),
        };
        let hashes = [
            compute_hooks_hash(&hooks(&[], &[], &[])),
            compute_hooks_hash(&hooks(&["npm install"], &[], &[])),
            compute_hooks_hash(&hooks(&["yarn install"], &[], &[])),
            compute_hooks_hash(&hooks(&[], &["npm install"], &[])),
            compute_hooks_hash(&hooks(&[], &[], &["npm install"])),
        ];
        let unique: std::collections::HashSet<_> = hashes.iter().collect();
        assert_eq!(unique.len(), hashes.len());
        assert_eq!(
            hashes[1],
            compute_hooks_hash(&hooks(&["npm install"], &[], &[]))
        );
    }

    #[test]
    fn trusted_repos_round_trip_and_legacy_rows() {
        let trusted = TrustedRepos {
            repos: vec![TrustedRepo {
                path: "/home/user/project".to_string(),
                hooks_hash: Some("abc123".to_string()),
                mcp_hash: Some("def456".to_string()),
                trusted_at: "2026-01-31T00:00:00Z".to_string(),
            }],
        };
        let serialized = toml::to_string_pretty(&trusted).unwrap();
        for line in [
            "path = \"/home/user/project\"",
            "hooks_hash = \"abc123\"",
            "mcp_hash = \"def456\"",
        ] {
            assert!(serialized.contains(line), "{serialized}");
        }
        let legacy = "[[repos]]\npath = \"/p\"\nhooks_hash = \"abc123\"\ntrusted_at = \"2026-01-31T00:00:00Z\"\n";
        let parsed: TrustedRepos = toml::from_str(legacy).unwrap();
        assert_eq!(parsed.repos[0].hooks_hash.as_deref(), Some("abc123"));
        assert_eq!(parsed.repos[0].mcp_hash, None);
    }
}
