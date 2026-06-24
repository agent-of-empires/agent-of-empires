//! External plugin install / update / uninstall, exercised in-process against
//! the library with an isolated app dir. Hermetic: GitHub sources clone a local
//! bare repo via `AOE_GITHUB_CLONE_BASE` and release assets come from a local
//! axum fixture via `AOE_UPDATE_API_BASE`. Never touches the network.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::plugin::install;
use agent_of_empires::plugin::lockfile::Lockfile;
use agent_of_empires::plugin::registry::PluginRegistry;
use agent_of_empires::session::Config;
use serial_test::serial;
use tempfile::TempDir;

/// Isolate the app dir under a fresh temp HOME for the duration of a test.
fn isolate() -> TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
    home
}

fn write_plugin_dir(parent: &Path, manifest: &str) -> PathBuf {
    let dir = parent.join("src-plugin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("aoe-plugin.toml"), manifest).unwrap();
    dir
}

fn load_registry() -> PluginRegistry {
    PluginRegistry::load(&Config::load().expect("config"))
}

fn git(args: &[&str], cwd: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Build a bare repo at `<base>/<owner>/<repo>.git` whose tree contains the
/// given files, and point `AOE_GITHUB_CLONE_BASE` at `<base>`.
fn make_bare_repo(base: &Path, owner: &str, repo: &str, files: &[(&str, &str)]) {
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&["init", "-q", "-b", "main"], &work);
    git(&["config", "user.email", "t@t.test"], &work);
    git(&["config", "user.name", "Test"], &work);
    for (name, contents) in files {
        std::fs::write(work.join(name), contents).unwrap();
    }
    git(&["add", "."], &work);
    git(&["commit", "-q", "-m", "init"], &work);

    let bare = base.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
        base,
    );
    std::env::set_var("AOE_GITHUB_CLONE_BASE", base);
}

#[tokio::test]
#[serial]
async fn local_install_lists_and_uninstalls() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.local"
name = "Local"
version = "0.1.0"
api_version = 2
"#,
    );

    let report = install::install(dir.to_str().unwrap(), true).await.unwrap();
    assert_eq!(report.id, "acme.local");
    assert!(report.granted);

    let reg = load_registry();
    let plugin = reg.get("acme.local").expect("installed plugin loads");
    assert!(!plugin.builtin());
    assert!(
        plugin.active(),
        "no-capability community plugin is active once installed"
    );
    assert_eq!(plugin.trust.as_str(), "community");
    assert!(Lockfile::load().unwrap().get("acme.local").is_some());

    install::uninstall("acme.local").unwrap();
    assert!(load_registry().get("acme.local").is_none());
    assert!(Lockfile::load().unwrap().get("acme.local").is_none());
}

#[tokio::test]
#[serial]
async fn reserved_namespace_is_rejected() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "aoe.evil"
name = "Evil"
version = "0.1.0"
api_version = 2
"#,
    );
    let err = install::install(dir.to_str().unwrap(), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("reserved namespace"), "got: {err}");
}

#[tokio::test]
#[serial]
async fn unknown_capability_is_rejected() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.future"
name = "Future"
version = "0.1.0"
api_version = 2
capabilities = ["totally.unknown"]
"#,
    );
    let err = install::install(dir.to_str().unwrap(), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not support"), "got: {err}");
}

#[tokio::test]
#[serial]
async fn grant_is_pinned_to_manifest_hash() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.caps"
name = "Caps"
version = "0.1.0"
api_version = 2
capabilities = ["net"]
"#,
    );
    install::install(dir.to_str().unwrap(), true).await.unwrap();
    assert!(load_registry().get("acme.caps").unwrap().active());

    // Tamper with the installed manifest so its hash changes; the grant no
    // longer covers it, so the plugin deactivates and needs re-approval.
    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.caps")
        .join("aoe-plugin.toml");
    let mut text = std::fs::read_to_string(&installed).unwrap();
    text.push_str("\n# tampered\n");
    std::fs::write(&installed, text).unwrap();

    let reg = load_registry();
    let plugin = reg.get("acme.caps").unwrap();
    assert!(!plugin.active(), "stale grant must deactivate the plugin");
    assert!(plugin.needs_reapproval());
}

#[tokio::test]
#[serial]
async fn github_source_clones_and_records_commit() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "widget",
        &[(
            "aoe-plugin.toml",
            r#"
id = "acme.widget"
name = "Widget"
version = "1.0.0"
api_version = 2
"#,
        )],
    );

    let report = install::install("gh:acme/widget", true).await.unwrap();
    assert_eq!(report.id, "acme.widget");

    let lock = Lockfile::load().unwrap();
    let locked = lock.get("acme.widget").expect("lock entry");
    assert_eq!(locked.source, "gh:acme/widget");
    assert!(
        locked
            .resolved_commit
            .as_deref()
            .is_some_and(|c| c.len() >= 7),
        "resolved commit recorded: {:?}",
        locked.resolved_commit
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn release_binary_is_downloaded_and_placed() {
    let _home = isolate();

    let asset_name = format!("bin-{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    // Fake GitHub API + asset download server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");
    let release_json = format!(
        r#"{{"tag_name":"v1.0.0","assets":[{{"name":"{asset_name}","browser_download_url":"{base_url}/dl"}}]}}"#
    );
    let app = axum::Router::new()
        .route(
            "/repos/acme/bin/releases/latest",
            axum::routing::get(move || {
                let body = release_json.clone();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            }),
        )
        .route(
            "/dl",
            axum::routing::get(|| async { b"#!/bin/sh\necho hi\n".to_vec() }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::env::set_var("AOE_UPDATE_API_BASE", &base_url);

    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "bin",
        &[(
            "aoe-plugin.toml",
            r#"
id = "acme.bin"
name = "Bin"
version = "1.0.0"
api_version = 2

[runtime]
kind = "release-binary"
asset = "bin-${os}-${arch}"
"#,
        )],
    );

    install::install("gh:acme/bin", true).await.unwrap();

    let placed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.bin")
        .join(&asset_name);
    assert!(
        placed.exists(),
        "release binary placed at {}",
        placed.display()
    );

    let lock = Lockfile::load().unwrap();
    let locked = lock.get("acme.bin").unwrap();
    assert_eq!(locked.release_tag.as_deref(), Some("v1.0.0"));
    assert_eq!(locked.asset_name.as_deref(), Some(asset_name.as_str()));
    assert!(locked
        .asset_sha256
        .as_deref()
        .is_some_and(|h| h.starts_with("sha256:")));

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
    std::env::remove_var("AOE_UPDATE_API_BASE");
    server.abort();
}
