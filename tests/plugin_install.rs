//! External plugin install / update / uninstall, exercised in-process against
//! the library with an isolated app dir. Hermetic: GitHub sources clone a local
//! bare repo via `AOE_GITHUB_CLONE_BASE` and release assets come from a local
//! axum fixture via `AOE_UPDATE_API_BASE`. Never touches the network.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::plugin::install::{self, UpdateOutcome};
use agent_of_empires::plugin::lockfile::Lockfile;
use agent_of_empires::plugin::registry::PluginRegistry;
use agent_of_empires::plugin::{auto_update, update_check};
use agent_of_empires::session::Config;
use serial_test::serial;
use tempfile::TempDir;

/// Isolate the app dir under a fresh temp HOME for the duration of a test.
///
/// Also clears `AOE_FEATURED_INDEX_PATH`: it is a process-global env var, and
/// these tests are `#[serial]`, so a featured test that aborts before its own
/// cleanup would otherwise leave a stale (deleted-tempdir) path that breaks
/// every later test. Clearing it at the start of each test makes the isolation
/// robust regardless of ordering or prior failures.
fn isolate() -> TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
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

/// Add a commit to the working tree behind a bare repo and push it, advancing
/// the remote `main` so an `ls-remote` check sees a newer commit.
fn push_new_commit(base: &Path, owner: &str, repo: &str, files: &[(&str, &str)]) {
    let work = base.join("work");
    for (name, contents) in files {
        std::fs::write(work.join(name), contents).unwrap();
    }
    git(&["add", "."], &work);
    git(&["commit", "-q", "-m", "update"], &work);
    let bare = base.join(owner).join(format!("{repo}.git"));
    git(&["push", "-q", bare.to_str().unwrap(), "main"], &work);
}

/// Tag the work tree behind a bare repo at its current HEAD and push the tag,
/// so a clone of `tag` resolves. `force` moves an existing tag to a new HEAD.
fn tag_bare_repo(base: &Path, owner: &str, repo: &str, tag: &str, force: bool) {
    let work = base.join("work");
    let mut args = vec!["tag"];
    if force {
        args.push("-f");
    }
    args.push(tag);
    git(&args, &work);
    let bare = base.join(owner).join(format!("{repo}.git"));
    let refspec = format!("refs/tags/{tag}:refs/tags/{tag}");
    git(
        &["push", "-q", "-f", bare.to_str().unwrap(), &refspec],
        &work,
    );
}

/// Spawn a fake GitHub releases API serving `tag` as the latest stable release
/// (and at `releases/tags/{tag}`), point `AOE_UPDATE_API_BASE` at it, and return
/// the server handle so the caller can abort it. Assets are empty (source-only
/// plugins); a release-binary test serves its own assets.
async fn spawn_latest_release(owner: &str, repo: &str, tag: &str) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let body = format!(r#"{{"tag_name":"{tag}","assets":[]}}"#);
    let json = move || {
        let body = body.clone();
        async move {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
        }
    };
    let app = axum::Router::new()
        .route(
            &format!("/repos/{owner}/{repo}/releases/latest"),
            axum::routing::get(json.clone()),
        )
        .route(
            &format!("/repos/{owner}/{repo}/releases/tags/{tag}"),
            axum::routing::get(json),
        );
    std::env::set_var("AOE_UPDATE_API_BASE", &base_url);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })
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
    assert_eq!(
        plugin.validation.as_str(),
        "local",
        "a local-directory install validates as local"
    );
    let locked = Lockfile::load().unwrap();
    let locked = locked.get("acme.local").expect("lock entry");
    assert!(
        locked.tree_hash.starts_with("sha256:"),
        "tree hash recorded: {:?}",
        locked.tree_hash
    );

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

    // An explicit `@ref` installs that ref directly (no release resolution);
    // --yes bypasses the unverified confirmation.
    let report = install::install("gh:acme/widget@main", true).await.unwrap();
    assert_eq!(report.id, "acme.widget");
    assert_eq!(
        report.validation.as_str(),
        "community",
        "the install report surfaces community trust for an unfeatured gh: install"
    );

    let lock = Lockfile::load().unwrap();
    let locked = lock.get("acme.widget").expect("lock entry");
    assert_eq!(locked.source, "gh:acme/widget");
    assert_eq!(locked.requested_ref.as_deref(), Some("main"));
    assert!(
        locked
            .resolved_commit
            .as_deref()
            .is_some_and(|c| c.len() >= 7),
        "resolved commit recorded: {:?}",
        locked.resolved_commit
    );
    assert!(
        locked.tree_hash.starts_with("sha256:"),
        "tree hash recorded: {:?}",
        locked.tree_hash
    );
    assert_eq!(
        load_registry()
            .get("acme.widget")
            .unwrap()
            .validation
            .as_str(),
        "community",
        "an unfeatured GitHub install validates as community"
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn github_no_ref_installs_latest_release() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "rel",
        &[(
            "aoe-plugin.toml",
            r#"
id = "acme.rel"
name = "Rel"
version = "1.0.0"
api_version = 2
"#,
        )],
    );
    tag_bare_repo(base.path(), "acme", "rel", "v1.0.0", false);
    let server = spawn_latest_release("acme", "rel", "v1.0.0").await;

    // No `@ref`: resolves and installs the latest release tag. `false` (no
    // --yes) proves the resolved-release path is not treated as unverified, so
    // it installs without an interactive confirmation.
    install::install("gh:acme/rel", false).await.unwrap();

    let lock = Lockfile::load().unwrap();
    let locked = lock.get("acme.rel").expect("lock entry");
    // The resolved release tag is recorded, but the config source stays ref-less
    // so `update` keeps tracking the latest-release channel (rolling).
    assert_eq!(locked.requested_ref.as_deref(), Some("v1.0.0"));
    assert_eq!(
        Config::load()
            .unwrap()
            .plugins
            .get("acme.rel")
            .and_then(|p| p.source.clone())
            .as_deref(),
        Some("gh:acme/rel"),
    );

    server.abort();
    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
    std::env::remove_var("AOE_UPDATE_API_BASE");
}

#[tokio::test]
#[serial]
async fn github_no_ref_no_release_bails_without_yes() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "norel",
        &[(
            "aoe-plugin.toml",
            r#"
id = "acme.norel"
name = "NoRel"
version = "1.0.0"
api_version = 2
"#,
        )],
    );
    // A releases API with no matching route returns 404 -> no release found ->
    // default-branch fallback, which is unverified and (non-interactively,
    // without --yes) must bail rather than silently install.
    let server = spawn_latest_release("other", "repo", "v9").await;

    let err = install::install("gh:acme/norel", false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unverified"), "got: {err}");
    assert!(load_registry().get("acme.norel").is_none());

    server.abort();
    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
    std::env::remove_var("AOE_UPDATE_API_BASE");
}

#[tokio::test]
#[serial]
async fn explicit_ref_requires_confirmation_without_yes() {
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

    // An explicit `@ref` is unverified; without --yes on a non-terminal stdin it
    // bails rather than installing un-audited code.
    let err = install::install("gh:acme/widget@main", false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unverified"), "got: {err}");
    assert!(load_registry().get("acme.widget").is_none());

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn outdated_tracks_release_not_default_branch() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "rel",
        &[(
            "aoe-plugin.toml",
            PLAIN_MANIFEST.replace("acme.upd", "acme.rel").as_str(),
        )],
    );
    tag_bare_repo(base.path(), "acme", "rel", "v1.0.0", false);
    let server = spawn_latest_release("acme", "rel", "v1.0.0").await;
    install::install("gh:acme/rel", true).await.unwrap();

    // Advance the default branch but leave the release tag at v1.0.0. A
    // release-tracking install must NOT report this as an update.
    push_new_commit(base.path(), "acme", "rel", &[("extra.txt", "new")]);
    let after = update_check::outdated().await;
    let s = after.iter().find(|s| s.id == "acme.rel").expect("present");
    assert!(
        !s.needs_update,
        "tracks the release tag, not default-branch HEAD: {s:?}"
    );
    assert!(s.error.is_none(), "no check error: {s:?}");

    server.abort();
    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
    std::env::remove_var("AOE_UPDATE_API_BASE");
}

#[tokio::test]
#[serial]
async fn featured_reserved_namespace_loads_after_tree_mutating_build() {
    // Regression for #2475: a featured plugin whose build mutates the install
    // tree (a `.venv`-style dir with a symlink) must still re-derive Featured at
    // load. Before the reserved-build-output skip, tree_hash hard-errored on the
    // symlink, so the plugin was not Featured and the reserved-namespace gate
    // skipped it.
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "agent-of-empires.official"
name = "Official"
version = "1.0.0"
api_version = 2
"#,
    );
    let tree_hash = agent_of_empires::plugin::integrity::tree_hash(&dir).unwrap();
    write_featured(
        src.path(),
        "agent-of-empires.official",
        dir.to_str().unwrap(),
        &tree_hash,
    );
    install::install(dir.to_str().unwrap(), true).await.unwrap();

    // Simulate a build that creates the reserved build-output dir with a symlink
    // inside the installed plugin, the way plugin-github's venv build would.
    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("agent-of-empires.official");
    let build = installed.join(".aoe-build").join("bin");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(build.join("real"), b"x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("real", build.join("python3")).unwrap();

    let reg = load_registry();
    let plugin = reg
        .get("agent-of-empires.official")
        .expect("featured plugin still loads after a tree-mutating build");
    assert_eq!(plugin.validation.as_str(), "featured");

    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
}

/// Write a featured index file and point `AOE_FEATURED_INDEX_PATH` at it (debug
/// builds only; tests run in debug). `versions` is a list of `(label, hash)`
/// vetted releases for the single entry.
fn write_featured_versions(
    dir: &Path,
    id: &str,
    source: &str,
    versions: &[(&str, &str)],
) -> PathBuf {
    let body: String = versions
        .iter()
        .map(|(label, hash)| format!("\"{label}\" = \"{hash}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let path = dir.join("featured.toml");
    std::fs::write(
        &path,
        format!("[plugins.\"{id}\"]\nsource = \"{source}\"\nversions = {{ {body} }}\n"),
    )
    .unwrap();
    std::env::set_var("AOE_FEATURED_INDEX_PATH", &path);
    path
}

/// Convenience: a single vetted release at `tree_hash`.
fn write_featured(dir: &Path, id: &str, source: &str, tree_hash: &str) -> PathBuf {
    write_featured_versions(dir, id, source, &[("1.0.0", tree_hash)])
}

#[tokio::test]
#[serial]
async fn featured_verified_reserved_namespace_installs() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    // A reserved-namespace id is normally rejected; a matching featured pin
    // lifts it.
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "agent-of-empires.official"
name = "Official"
version = "1.0.0"
api_version = 2
"#,
    );
    let tree_hash = agent_of_empires::plugin::integrity::tree_hash(&dir).unwrap();
    write_featured(
        src.path(),
        "agent-of-empires.official",
        dir.to_str().unwrap(),
        &tree_hash,
    );

    install::install(dir.to_str().unwrap(), true).await.unwrap();

    let reg = load_registry();
    let plugin = reg.get("agent-of-empires.official").expect("installed");
    assert_eq!(plugin.validation.as_str(), "featured");
    let lock = Lockfile::load().unwrap();
    let locked = lock.get("agent-of-empires.official").unwrap();
    assert_eq!(locked.trust, "featured");
    assert_eq!(locked.tree_hash, tree_hash);

    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
}

#[tokio::test]
#[serial]
async fn featured_unvetted_version_installs_as_community() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.featured"
name = "Featured"
version = "1.0.0"
api_version = 2
"#,
    );
    // The id is featured but pinned to a different (unvetted) hash. For a
    // non-reserved id this is not tamper-refuse: it installs as an unvetted
    // version (community).
    write_featured(
        src.path(),
        "acme.featured",
        dir.to_str().unwrap(),
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    );

    let report = install::install(dir.to_str().unwrap(), true).await.unwrap();
    assert_eq!(
        report.validation.as_str(),
        "local",
        "the install report surfaces a local-directory install as local"
    );

    // It installs (not refused) and is not featured. The non-featured label
    // ("local" here, since the install source is a local dir; "community" for a
    // gh: install) is derived from the source, not the hash mismatch.
    let reg = load_registry();
    let plugin = reg.get("acme.featured").expect("installed");
    assert_ne!(plugin.validation.as_str(), "featured");
    assert_eq!(plugin.validation.as_str(), "local");

    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
}

#[tokio::test]
#[serial]
async fn featured_second_vetted_version_verifies() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "agent-of-empires.official"
name = "Official"
version = "1.1.0"
api_version = 2
"#,
    );
    let tree_hash = agent_of_empires::plugin::integrity::tree_hash(&dir).unwrap();
    // The actual tree is the second vetted release; an earlier release is also
    // listed and must not un-verify this one.
    write_featured_versions(
        src.path(),
        "agent-of-empires.official",
        dir.to_str().unwrap(),
        &[
            (
                "1.0.0",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ),
            ("1.1.0", &tree_hash),
        ],
    );

    install::install(dir.to_str().unwrap(), true).await.unwrap();

    let reg = load_registry();
    let plugin = reg.get("agent-of-empires.official").expect("installed");
    assert_eq!(plugin.validation.as_str(), "featured");

    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
}

#[tokio::test]
#[serial]
async fn featured_reserved_namespace_unvetted_is_refused() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    // A reserved-namespace id at an unvetted hash is still refused: only a
    // vetted release lifts the reserved-namespace gate.
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "agent-of-empires.official"
name = "Official"
version = "1.0.0"
api_version = 2
"#,
    );
    write_featured(
        src.path(),
        "agent-of-empires.official",
        dir.to_str().unwrap(),
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    );

    let err = install::install(dir.to_str().unwrap(), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("reserved"), "got: {err}");
    assert!(load_registry().get("agent-of-empires.official").is_none());

    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
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
    let json_handler = move || {
        let body = release_json.clone();
        async move {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
        }
    };
    let app = axum::Router::new()
        // No `@ref` resolves the latest release tag, then pins source + asset to
        // it, so both the latest and the by-tag endpoints are hit.
        .route(
            "/repos/acme/bin/releases/latest",
            axum::routing::get(json_handler.clone()),
        )
        .route(
            "/repos/acme/bin/releases/tags/v1.0.0",
            axum::routing::get(json_handler),
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
    tag_bare_repo(base.path(), "acme", "bin", "v1.0.0", false);

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

/// Build steps run in the FINAL installed directory (not the staging tree that
/// is renamed away), so a build artifact lands at `<plugins_dir>/<id>`. Uses a
/// bare `sh` launch command (resolves on PATH) so install's post-build
/// entrypoint check passes without the build having to produce an executable.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn build_step_runs_in_final_dir() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.built"
name = "Built"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["cp", "aoe-plugin.toml", "build-marker"]
"#,
    );

    install::install(dir.to_str().unwrap(), true).await.unwrap();

    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.built");
    assert!(
        installed.join("build-marker").exists(),
        "build step ran with cwd = the final plugin dir"
    );
}

/// A build step whose `platforms` excludes the host OS is skipped, so its
/// artifact is never produced and install still succeeds.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn build_step_skipped_on_non_matching_platform() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.skip"
name = "Skip"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["cp", "aoe-plugin.toml", "should-not-exist"]
platforms = ["windows"]
"#,
    );

    install::install(dir.to_str().unwrap(), true).await.unwrap();

    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.skip");
    assert!(
        installed.exists(),
        "install succeeds with all steps skipped"
    );
    assert!(
        !installed.join("should-not-exist").exists(),
        "a platform-mismatched build step does not run"
    );
}

/// A `system = true` worker resolves its program on PATH at launch, not at
/// install. Install must not gate on the install shell's PATH (which is not the
/// daemon's PATH), so a system-tool entrypoint absent from the install
/// environment still installs and is left to resolve at launch.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn system_worker_absent_from_install_path_still_installs() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.systool"
name = "SysTool"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = ["aoe-definitely-not-on-path-xyz", "run", "worker"]
system = true
"#,
    );

    install::install(dir.to_str().unwrap(), true)
        .await
        .expect("system-tool worker installs without an install-time PATH check");

    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.systool");
    assert!(installed.exists(), "plugin dir is in place");
    assert!(load_registry().get("acme.systool").is_some());
}

/// A failing build aborts the install and leaves no trace: no installed
/// directory, no registry entry, no lockfile entry.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn failed_build_aborts_install_and_cleans_up() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.failbuild"
name = "FailBuild"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["false"]
"#,
    );

    let err = install::install(dir.to_str().unwrap(), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("build step"), "got: {err}");

    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.failbuild");
    assert!(!installed.exists(), "no half-installed dir left behind");
    assert!(load_registry().get("acme.failbuild").is_none());
    assert!(Lockfile::load().unwrap().get("acme.failbuild").is_none());
}

/// A failing build during update restores the previously installed version
/// instead of leaving the user with a broken plugin.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn failed_update_build_restores_prior_version() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();

    // v1 installs cleanly and produces a build artifact.
    write_plugin_dir(
        src.path(),
        r#"
id = "acme.upd"
name = "Upd"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["cp", "aoe-plugin.toml", "v1-marker"]
"#,
    );
    let dir = src.path().join("src-plugin");
    install::install(dir.to_str().unwrap(), true).await.unwrap();
    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.upd");
    assert!(installed.join("v1-marker").exists());

    // v2 at the same source now has a failing build.
    std::fs::write(
        dir.join("aoe-plugin.toml"),
        r#"
id = "acme.upd"
name = "Upd"
version = "0.2.0"
api_version = 2

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["false"]
"#,
    )
    .unwrap();

    let err = install::update("acme.upd").await.unwrap_err().to_string();
    assert!(err.contains("build step"), "got: {err}");

    // The prior install is intact: directory, artifact, and recorded version.
    assert!(
        installed.join("v1-marker").exists(),
        "v1 build artifact restored after failed update"
    );
    assert!(load_registry().get("acme.upd").is_some());
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "0.1.0",
        "lockfile still records the working version"
    );
    // No leftover backup directory from the failed update.
    assert!(!installed.with_file_name("acme.upd.bak").exists());
}

/// A changed build recipe on update must re-prompt even when capabilities are
/// unchanged, so a modified (possibly malicious) build cannot run unattended.
/// Non-interactively that prompt bails, leaving the prior version untouched.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn update_reprompts_when_build_recipe_changes() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();

    write_plugin_dir(
        src.path(),
        r#"
id = "acme.recipe"
name = "Recipe"
version = "0.1.0"
api_version = 2
capabilities = ["net"]

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["cp", "aoe-plugin.toml", "marker-v1"]
"#,
    );
    let dir = src.path().join("src-plugin");
    install::install(dir.to_str().unwrap(), true).await.unwrap();

    // Same capability, but the build recipe changed.
    std::fs::write(
        dir.join("aoe-plugin.toml"),
        r#"
id = "acme.recipe"
name = "Recipe"
version = "0.2.0"
api_version = 2
capabilities = ["net"]

[runtime]
kind = "command"
command = ["sh"]
system = true

[[runtime.build]]
command = ["cp", "aoe-plugin.toml", "marker-v2"]
"#,
    )
    .unwrap();

    // The changed recipe forces a prompt, which bails on non-terminal stdin
    // instead of silently running the new build.
    let err = install::update("acme.recipe")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a terminal"), "got: {err}");

    // Prior version is fully intact: v1 artifact present, v2 never ran.
    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.recipe");
    assert!(installed.join("marker-v1").exists());
    assert!(!installed.join("marker-v2").exists());
    assert_eq!(
        Lockfile::load()
            .unwrap()
            .get("acme.recipe")
            .unwrap()
            .version,
        "0.1.0"
    );
}

const PLAIN_MANIFEST: &str = r#"
id = "acme.upd"
name = "Upd"
version = "1.0.0"
api_version = 2
"#;

#[tokio::test]
#[serial]
async fn outdated_detects_new_github_commit() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "upd",
        &[("aoe-plugin.toml", PLAIN_MANIFEST)],
    );
    // Branch tracking is now an explicit-ref opt-in: `@main` follows the default
    // branch (no release resolution), which these update-mechanics tests need.
    install::install("gh:acme/upd@main", true).await.unwrap();

    let before = update_check::outdated().await;
    let s = before.iter().find(|s| s.id == "acme.upd").expect("present");
    assert!(!s.needs_update, "fresh install is current: {s:?}");
    assert!(s.error.is_none(), "no check error: {s:?}");

    // Advance the remote; the same manifest stays so it is a clean update.
    push_new_commit(base.path(), "acme", "upd", &[("extra.txt", "new")]);
    let after = update_check::outdated().await;
    let s = after.iter().find(|s| s.id == "acme.upd").expect("present");
    assert!(s.needs_update, "new commit detected: {s:?}");

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn outdated_detects_changed_local_tree() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(src.path(), PLAIN_MANIFEST);
    install::install(dir.to_str().unwrap(), true).await.unwrap();

    let before = update_check::outdated().await;
    assert!(
        !before
            .iter()
            .find(|s| s.id == "acme.upd")
            .unwrap()
            .needs_update
    );

    // Edit the local source tree; the re-hash should diverge from the lock.
    std::fs::write(dir.join("added.txt"), "changed").unwrap();
    let after = update_check::outdated().await;
    let s = after.iter().find(|s| s.id == "acme.upd").expect("present");
    assert!(s.needs_update, "local tree change detected: {s:?}");
}

#[tokio::test]
#[serial]
async fn auto_update_applies_clean_github_update() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    let v2 = PLAIN_MANIFEST.replace("1.0.0", "2.0.0");
    make_bare_repo(
        base.path(),
        "acme",
        "upd",
        &[("aoe-plugin.toml", PLAIN_MANIFEST)],
    );
    // Branch tracking is now an explicit-ref opt-in: `@main` follows the default
    // branch (no release resolution), which these update-mechanics tests need.
    install::install("gh:acme/upd@main", true).await.unwrap();

    // A clean (no consent change) newer version on the remote.
    push_new_commit(base.path(), "acme", "upd", &[("aoe-plugin.toml", &v2)]);
    let summary = auto_update::sweep().await;
    assert_eq!(summary.applied, vec!["acme.upd".to_string()], "{summary:?}");
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "2.0.0",
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn clean_update_skips_capability_change() {
    let _home = isolate();
    let base = tempfile::tempdir().unwrap();
    // Bump the version too, so the "prior version kept" assertion actually
    // proves nothing was rewritten (a same-version skip could pass vacuously).
    let with_cap = PLAIN_MANIFEST.replace("1.0.0", "2.0.0").replace(
        "api_version = 2",
        "api_version = 2\ncapabilities = [\"net\"]",
    );
    make_bare_repo(
        base.path(),
        "acme",
        "upd",
        &[("aoe-plugin.toml", PLAIN_MANIFEST)],
    );
    // Branch tracking is now an explicit-ref opt-in: `@main` follows the default
    // branch (no release resolution), which these update-mechanics tests need.
    install::install("gh:acme/upd@main", true).await.unwrap();

    // The new version adds a capability, so a non-interactive clean update must
    // skip it and leave the prior version installed.
    push_new_commit(
        base.path(),
        "acme",
        "upd",
        &[("aoe-plugin.toml", &with_cap)],
    );
    match install::update_clean("acme.upd").await.unwrap() {
        UpdateOutcome::Skipped { id, .. } => assert_eq!(id, "acme.upd"),
        other => panic!("expected skip on capability change, got {other:?}"),
    }
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "1.0.0",
        "prior version kept",
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}
