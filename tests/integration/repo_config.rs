//! Integration tests for repo config loading, the trust system, and resolving
//! the repo layer against saved profile config.

use agent_of_empires::session::config::repo_config::{
    check_repo_trust, load_repo_config, resolve_config_with_repo, trust_repo, TrustSurface,
    INIT_TEMPLATE,
};
use serial_test::serial;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use crate::common::set_temp_home;

/// Write `content` to `<tmp>/<dir>/config.toml` for each `(dir, content)`.
fn repo_with(files: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for (dir, content) in files {
        let config_dir = tmp.path().join(dir);
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), content).unwrap();
    }
    tmp
}

fn setup_repo_config(content: &str) -> TempDir {
    repo_with(&[(".agent-of-empires", content)])
}

/// Which file is read, and what an empty or all-commented file yields:
/// `(files, expected on_create hooks)`, `None` meaning no repo layer at all.
#[test]
#[serial_test::parallel]
fn load_repo_config_picks_the_file_and_skips_empty_ones() {
    let hooks_toml = |cmd: &str| format!("[hooks]\non_create = [\"{cmd}\"]\n");
    let new = hooks_toml("echo new");
    let legacy = hooks_toml("echo legacy");
    let cases: [(Vec<(&str, &str)>, Option<Option<&str>>); 5] = [
        (vec![(".agent-of-empires", "")], None),
        (vec![(".agent-of-empires", INIT_TEMPLATE)], Some(None)),
        (vec![(".agent-of-empires", &new)], Some(Some("echo new"))),
        (vec![(".aoe", &legacy)], Some(Some("echo legacy"))),
        (
            vec![(".agent-of-empires", &new), (".aoe", &legacy)],
            Some(Some("echo new")),
        ),
    ];
    for (files, expected) in cases {
        let repo = repo_with(&files);
        let config = load_repo_config(repo.path()).unwrap();
        let hooks = config.map(|c| c.hooks().map(|h| h.on_create));
        assert_eq!(
            hooks,
            expected.map(|e| e.map(|cmd| vec![cmd.to_string()])),
            "{files:?}"
        );
    }
}

/// Trust is pinned to the hooks hash: editing the hooks revokes it until the
/// new content is trusted, and trusting the new hash drops the old one.
#[test]
#[serial]
fn hook_trust_follows_the_config_content() {
    let temp_home = TempDir::new().unwrap();
    let _home = set_temp_home(temp_home.path());

    let repo = setup_repo_config("[hooks]\non_create = [\"echo v1\"]\n");
    let needs_trust = |path: &Path| match check_repo_trust(path).unwrap().hooks {
        TrustSurface::NeedsTrust { hash, .. } => Some(hash),
        TrustSurface::Trusted(_) => None,
        TrustSurface::Absent => panic!("hooks surface must be present"),
    };

    let v1 = needs_trust(repo.path()).expect("new hooks need trust");
    trust_repo(repo.path(), Some(&v1), None).unwrap();
    assert_eq!(needs_trust(repo.path()), None);

    fs::write(
        repo.path().join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"echo v1\", \"echo v2\"]\n",
    )
    .unwrap();
    let v2 = needs_trust(repo.path()).expect("edited hooks need re-trust");
    assert_ne!(v1, v2);
    trust_repo(repo.path(), Some(&v2), None).unwrap();
    assert_eq!(needs_trust(repo.path()), None);

    fs::write(
        repo.path().join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"echo v1\"]\n",
    )
    .unwrap();
    assert_eq!(
        needs_trust(repo.path()),
        Some(v1),
        "re-trusting replaces the old hash"
    );
}

/// The repo layer reaches the resolved config for the fields a repo may set
/// (#557: `volume_ignores` was silently dropped, `auto_cleanup`), while host
/// access (`extra_volumes`, `mount_ssh` #3154, `environment` #3710) and
/// worktree placement (#3711) keep the profile's values.
#[test]
#[serial]
fn repo_layer_resolves_only_repo_settable_fields() {
    let temp_home = TempDir::new().unwrap();
    let _home = set_temp_home(temp_home.path());

    let profile: agent_of_empires::session::ProfileConfig =
        serde_json::from_value(serde_json::json!({
            "sandbox": {"environment": ["GH_TOKEN=$AOE_GH_TOKEN"]},
            "worktree": {"bare_repo_path_template": "../{branch}"}
        }))
        .unwrap();
    agent_of_empires::session::save_profile_config("default", &profile).unwrap();

    let repo = setup_repo_config(
        r#"
[sandbox]
volume_ignores = [".venv", "node_modules"]
environment = ["AWS_SECRET_ACCESS_KEY", "CI=$HOME"]
extra_volumes = ["/data:/data:ro"]
mount_ssh = true

[worktree]
enabled = true
path_template = "/tmp/{branch}"
bare_repo_path_template = "../../{branch}"
workspace_path_template = "/tmp/ws-{branch}"
auto_cleanup = false
"#,
    );

    let config = resolve_config_with_repo("default", repo.path()).unwrap();
    let defaults = agent_of_empires::session::config::WorktreeConfig::default();

    assert_eq!(config.sandbox.volume_ignores, vec![".venv", "node_modules"]);
    assert_eq!(config.sandbox.environment, vec!["GH_TOKEN=$AOE_GH_TOKEN"]);
    assert!(config.sandbox.extra_volumes.is_empty());
    assert!(!config.sandbox.mount_ssh);
    assert_eq!(config.worktree.bare_repo_path_template, "../{branch}");
    assert!(!config.worktree.enabled);
    assert_eq!(config.worktree.path_template, defaults.path_template);
    assert_eq!(
        config.worktree.workspace_path_template,
        defaults.workspace_path_template
    );
    assert!(
        !config.worktree.auto_cleanup,
        "auto_cleanup is repo-settable"
    );
}

/// #3400: on macOS and Windows the app dir is `~/.agent-of-empires`, so a
/// project path of `$HOME` makes `<project>/.agent-of-empires/config.toml`
/// resolve to the user's own global config. Neither side of the repo layer may
/// act on that file: reading it re-merges the global layer onto itself (and
/// reports every global-only section as a rejected repo override), and saving
/// truncates the global config to the repo-permitted subset.
///
/// A debug build namespaces the app dir (`.agent-of-empires-dev`), so `$HOME`
/// alone cannot produce the collision here. Symlinking the project's
/// `.agent-of-empires` at the app dir yields the identical resolved file, which
/// is what both guards compare.
#[test]
#[serial]
#[cfg(unix)]
fn test_project_path_that_resolves_to_global_config_is_not_a_repo_config() {
    use agent_of_empires::session::config::repo_config::{
        load_repo_config, save_repo_config, RepoConfig,
    };

    let temp_home = TempDir::new().unwrap();
    let _home = set_temp_home(temp_home.path());

    let app_dir = agent_of_empires::session::get_app_dir().unwrap();
    let global_config = app_dir.join("config.toml");
    let global_content =
        "[session]\ndefault_tool = \"claude\"\n\n[logging]\ndefault_level = \"debug\"\n";
    fs::write(&global_config, global_content).unwrap();

    let project = temp_home.path().join("project");
    fs::create_dir_all(&project).unwrap();
    std::os::unix::fs::symlink(&app_dir, project.join(".agent-of-empires")).unwrap();

    assert!(
        load_repo_config(&project).unwrap().is_none(),
        "the global config.toml must not be loaded as a repo config layer"
    );

    let repo_config: RepoConfig = toml::from_str("[session]\ndefault_tool = \"codex\"\n").unwrap();
    let err = save_repo_config(&project, &repo_config).unwrap_err();
    assert!(
        err.to_string().contains("global config.toml"),
        "save should name the collision, got: {err}"
    );
    assert_eq!(
        fs::read_to_string(&global_config).unwrap(),
        global_content,
        "the global config must be left byte-identical"
    );
}

/// An empty project path means "this session has no project repo" (scratch
/// sessions pass one, see `session::builder::build_instance`). It must carry no
/// repo layer at all: `Path::new("")` joins to the *relative*
/// `.agent-of-empires/config.toml`, which resolves against whatever directory
/// the process happens to be running in.
#[test]
#[serial]
fn test_empty_project_path_ignores_the_launch_directory() {
    let tmp = setup_repo_config("[session]\ndefault_tool = \"codex\"\n");
    let cwd = crate::common::CwdGuard::set(tmp.path());
    let loaded =
        agent_of_empires::session::config::repo_config::load_repo_config(std::path::Path::new(""));
    drop(cwd);

    assert!(
        loaded.unwrap().is_none(),
        "an empty project path must not pick up the launch directory's repo config"
    );

    // The real callers reach the loader through `repo_config_source_path`, and
    // its `.git` probe is relative too. From a linked worktree (whose `.git` is
    // a file) it resolves to that worktree's main repo, which would hand the
    // loader a non-empty path and reinstate the repo layer.
    let main = setup_repo_config("[session]\ndefault_tool = \"codex\"\n");
    fs::create_dir_all(main.path().join(".git/worktrees/wt")).unwrap();
    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}/.git/worktrees/wt\n", main.path().display()),
    )
    .unwrap();

    let cwd = crate::common::CwdGuard::set(worktree.path());
    let source = agent_of_empires::session::config::repo_config::repo_config_source_path(
        std::path::Path::new(""),
    );
    let loaded = agent_of_empires::session::config::repo_config::load_repo_config(&source);
    drop(cwd);

    assert_eq!(
        source,
        std::path::PathBuf::new(),
        "an empty project path must not resolve to the launch worktree's main repo"
    );
    assert!(
        loaded.unwrap().is_none(),
        "an empty project path must carry no repo layer from a worktree launch dir"
    );
}
