//! Integration tests for repo config loading, trust system, and hook execution.

use serial_test::serial;
use std::fs;
use tempfile::TempDir;

use crate::common::set_temp_home;

/// Helper to set up a temp dir with `.agent-of-empires/config.toml`.
fn setup_repo_config(content: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join(".agent-of-empires");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), content).unwrap();
    tmp
}

/// Helper to set up a temp dir with legacy `.aoe/config.toml`.
fn setup_legacy_repo_config(content: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let aoe_dir = tmp.path().join(".aoe");
    fs::create_dir_all(&aoe_dir).unwrap();
    fs::write(aoe_dir.join("config.toml"), content).unwrap();
    tmp
}

#[test]
fn test_load_repo_config_from_temp_dir() {
    let tmp = setup_repo_config(
        r#"
[hooks]
on_create = ["echo setup"]
on_launch = ["echo start"]

[session]
default_tool = "claude"
"#,
    );

    let config = agent_of_empires::session::repo_config::load_repo_config(tmp.path())
        .unwrap()
        .unwrap();

    let hooks = config.hooks().unwrap();
    assert_eq!(hooks.on_create, vec!["echo setup"]);
    assert_eq!(hooks.on_launch, vec!["echo start"]);
    let ov = serde_json::to_value(&config).unwrap();
    assert_eq!(ov["session"]["default_tool"], serde_json::json!("claude"));
}

#[test]
fn test_load_repo_config_empty_file() {
    let tmp = setup_repo_config("");
    let config = agent_of_empires::session::repo_config::load_repo_config(tmp.path()).unwrap();
    assert!(config.is_none());
}

#[test]
fn test_load_repo_config_comments_only() {
    let tmp = setup_repo_config(agent_of_empires::session::repo_config::INIT_TEMPLATE);
    let config = agent_of_empires::session::repo_config::load_repo_config(tmp.path())
        .unwrap()
        .unwrap();
    // All-commented template should parse as empty config
    assert!(config.hooks().is_none());
    assert!(!config.overrides.contains_key("session"));
}

#[test]
#[serial]
fn test_trust_untrust_cycle() {
    let temp_home = TempDir::new().unwrap();
    set_temp_home(temp_home.path());

    let project_dir = TempDir::new().unwrap();
    let project_path = project_dir.path();
    let hooks_hash = "test_hash_123";

    use agent_of_empires::session::repo_config::{is_repo_trusted, trust_repo};

    // Initially not trusted
    assert!(!is_repo_trusted(project_path, Some(hooks_hash), None).unwrap());

    // Trust it
    trust_repo(project_path, Some(hooks_hash), None).unwrap();
    assert!(is_repo_trusted(project_path, Some(hooks_hash), None).unwrap());

    // Different hash should not be trusted
    assert!(!is_repo_trusted(project_path, Some("different_hash"), None).unwrap());

    // Re-trust with new hash (simulating hooks changed)
    trust_repo(project_path, Some("new_hash"), None).unwrap();
    // Old hash no longer trusted
    assert!(!is_repo_trusted(project_path, Some(hooks_hash), None).unwrap());
    // New hash is trusted
    assert!(is_repo_trusted(project_path, Some("new_hash"), None).unwrap());
}

#[test]
fn test_hook_execution_simple_echo() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("hook_ran");

    let cmd = format!("touch {}", marker.display());
    agent_of_empires::session::repo_config::execute_hooks(&[cmd], tmp.path(), &[]).unwrap();

    assert!(marker.exists());
}

#[test]
fn test_hook_execution_failure() {
    let tmp = TempDir::new().unwrap();
    let result = agent_of_empires::session::repo_config::execute_hooks(
        &["exit 1".to_string()],
        tmp.path(),
        &[],
    );
    assert!(result.is_err());
}

#[test]
fn test_changed_hooks_invalidate_trust() {
    use agent_of_empires::session::repo_config::{compute_hooks_hash, HooksConfig};

    let hooks_v1 = HooksConfig {
        on_create: vec!["npm install".to_string()],
        ..Default::default()
    };
    let hooks_v2 = HooksConfig {
        on_create: vec!["npm install".to_string(), "npm run build".to_string()],
        ..Default::default()
    };

    let hash_v1 = compute_hooks_hash(&hooks_v1);
    let hash_v2 = compute_hooks_hash(&hooks_v2);
    assert_ne!(
        hash_v1, hash_v2,
        "different hooks should produce different hashes"
    );
}

#[test]
#[serial]
fn test_hook_trust_invalidated_on_config_change() {
    use agent_of_empires::session::repo_config::{check_repo_trust, trust_repo, TrustSurface};

    let temp_home = TempDir::new().unwrap();
    set_temp_home(temp_home.path());

    // Create a repo with hooks
    let repo = setup_repo_config(
        r#"
[hooks]
on_create = ["echo setup"]
"#,
    );

    // Initially untrusted
    let trust = check_repo_trust(repo.path()).unwrap();
    let hash = match &trust.hooks {
        TrustSurface::NeedsTrust { hash, .. } => hash.clone(),
        _ => panic!("Hooks should initially need trust"),
    };

    // Trust the hooks
    trust_repo(repo.path(), Some(&hash), None).unwrap();

    // Now should be trusted
    let trust = check_repo_trust(repo.path()).unwrap();
    assert!(
        matches!(trust.hooks, TrustSurface::Trusted(_)),
        "Hooks should be trusted after trust_repo"
    );

    // Modify the hooks config
    let config_dir = repo.path().join(".agent-of-empires");
    fs::write(
        config_dir.join("config.toml"),
        r#"
[hooks]
on_create = ["echo setup", "echo extra"]
"#,
    )
    .unwrap();

    // Should no longer be trusted (hash changed)
    let trust = check_repo_trust(repo.path()).unwrap();
    assert!(
        trust.hooks.needs_trust(),
        "Modified hooks should need re-trust"
    );
}

#[test]
#[serial]
fn test_hook_re_trust_after_change() {
    use agent_of_empires::session::repo_config::{check_repo_trust, trust_repo, TrustSurface};

    let temp_home = TempDir::new().unwrap();
    set_temp_home(temp_home.path());

    let repo = setup_repo_config(
        r#"
[hooks]
on_create = ["echo v1"]
"#,
    );

    // Trust v1
    let trust = check_repo_trust(repo.path()).unwrap();
    let hash = match &trust.hooks {
        TrustSurface::NeedsTrust { hash, .. } => hash.clone(),
        _ => panic!("v1 hooks should initially need trust"),
    };
    trust_repo(repo.path(), Some(&hash), None).unwrap();

    // Modify to v2
    let config_dir = repo.path().join(".agent-of-empires");
    fs::write(
        config_dir.join("config.toml"),
        r#"
[hooks]
on_create = ["echo v2"]
"#,
    )
    .unwrap();

    // Re-trust v2
    let trust = check_repo_trust(repo.path()).unwrap();
    assert!(trust.hooks.needs_trust());
    if let TrustSurface::NeedsTrust { hash, .. } = &trust.hooks {
        trust_repo(repo.path(), Some(hash), None).unwrap();
    }

    // Should now be trusted again
    let trust = check_repo_trust(repo.path()).unwrap();
    assert!(
        matches!(trust.hooks, TrustSurface::Trusted(_)),
        "Re-trusted hooks should be trusted"
    );
}

/// Regression test for #557: repo-level sandbox config (environment, volume_ignores,
/// extra_volumes) must be included in the resolved config, not silently dropped.
#[test]
#[serial]
fn test_repo_sandbox_config_merged_into_resolved_config() {
    let temp_home = TempDir::new().unwrap();
    set_temp_home(temp_home.path());

    let repo = setup_repo_config(
        r#"
[sandbox]
volume_ignores = [".venv", "node_modules"]
environment = ["CI=true", "MY_VAR=hello"]
extra_volumes = ["/data:/data:ro"]
mount_ssh = true
"#,
    );

    let config =
        agent_of_empires::session::repo_config::resolve_config_with_repo("default", repo.path())
            .unwrap();

    assert_eq!(
        config.sandbox.volume_ignores,
        vec![".venv", "node_modules"],
        "volume_ignores from repo config should be present"
    );
    assert_eq!(
        config.sandbox.environment,
        vec!["CI=true", "MY_VAR=hello"],
        "environment from repo config should be present"
    );
    assert_eq!(
        config.sandbox.extra_volumes,
        vec!["/data:/data:ro"],
        "extra_volumes from repo config should be present"
    );
    assert!(
        config.sandbox.mount_ssh,
        "mount_ssh from repo config should be true"
    );
}

/// Regression test for #568: repo-level bare_repo_path_template must be included
/// in the resolved config, not silently dropped.
#[test]
#[serial]
fn test_repo_worktree_config_merged_into_resolved_config() {
    let temp_home = TempDir::new().unwrap();
    set_temp_home(temp_home.path());

    let repo = setup_repo_config(
        r#"
[worktree]
bare_repo_path_template = "../{branch}"
"#,
    );

    let config =
        agent_of_empires::session::repo_config::resolve_config_with_repo("default", repo.path())
            .unwrap();

    assert_eq!(
        config.worktree.bare_repo_path_template, "../{branch}",
        "bare_repo_path_template from repo config should override the default"
    );
}

/// Legacy `.aoe/config.toml` should still be loaded via backwards compat fallback.
#[test]
fn test_legacy_aoe_path_still_loads() {
    let repo = setup_legacy_repo_config(
        r#"
[hooks]
on_create = ["echo legacy"]
"#,
    );

    let config = agent_of_empires::session::repo_config::load_repo_config(repo.path())
        .unwrap()
        .unwrap();

    let hooks = config.hooks().unwrap();
    assert_eq!(hooks.on_create, vec!["echo legacy"]);
}

/// New `.agent-of-empires/config.toml` takes priority over legacy `.aoe/config.toml`.
#[test]
fn test_new_path_takes_priority_over_legacy() {
    let tmp = TempDir::new().unwrap();

    // Create both paths with different content
    let new_dir = tmp.path().join(".agent-of-empires");
    fs::create_dir_all(&new_dir).unwrap();
    fs::write(
        new_dir.join("config.toml"),
        r#"
[hooks]
on_create = ["echo new"]
"#,
    )
    .unwrap();

    let legacy_dir = tmp.path().join(".aoe");
    fs::create_dir_all(&legacy_dir).unwrap();
    fs::write(
        legacy_dir.join("config.toml"),
        r#"
[hooks]
on_create = ["echo legacy"]
"#,
    )
    .unwrap();

    let config = agent_of_empires::session::repo_config::load_repo_config(tmp.path())
        .unwrap()
        .unwrap();

    let hooks = config.hooks().unwrap();
    assert_eq!(
        hooks.on_create,
        vec!["echo new"],
        "new path should take priority over legacy"
    );
}
