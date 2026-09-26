//! External plugin install / update / uninstall, exercised in-process against
//! the library with an isolated app dir. Hermetic: GitHub sources clone a local
//! bare repo via `AOE_GITHUB_CLONE_BASE` and release assets come from a local
//! axum fixture via `AOE_UPDATE_API_BASE`. Never touches the network.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_of_empires::plugin::install::{self, UpdatePreview};
use agent_of_empires::plugin::lockfile::Lockfile;
use agent_of_empires::plugin::registry::PluginRegistry;
use agent_of_empires::plugin::{auto_update, update_check};
use agent_of_empires::session::Config;
use serial_test::serial;
use tempfile::TempDir;

fn isolate() -> crate::common::TestHome {
    let mut home = crate::common::setup_temp_home();
    home.env = home.env.and_set("AOE_FEATURED_INDEX_PATH", "");
    std::env::remove_var("AOE_FEATURED_INDEX_PATH");
    home.env = home.env.and_set("AOE_GITHUB_CLONE_BASE", "");
    home.env = home.env.and_set("AOE_UPDATE_API_BASE", "");
    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
    std::env::remove_var("AOE_UPDATE_API_BASE");
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

    let needs_update = || async {
        update_check::outdated()
            .await
            .into_iter()
            .find(|s| s.id == "acme.local")
            .expect("present")
            .needs_update
    };
    assert!(!needs_update().await, "fresh install is current");
    // Editing the local source tree diverges its re-hash from the lock.
    std::fs::write(dir.join("added.txt"), "changed").unwrap();
    assert!(needs_update().await, "local tree change detected");

    install::uninstall("acme.local").unwrap();
    assert!(load_registry().get("acme.local").is_none());
    assert!(Lockfile::load().unwrap().get("acme.local").is_none());
}

/// Install refuses a manifest in the reserved `aoe.` namespace and one that
/// asks for a capability this build does not support.
#[tokio::test]
#[serial]
async fn invalid_manifests_are_rejected() {
    let _home = isolate();
    for (manifest, expected) in [
        (
            "id = \"aoe.evil\"\nname = \"Evil\"\nversion = \"0.1.0\"\napi_version = 2\n",
            "reserved namespace",
        ),
        (
            "id = \"acme.future\"\nname = \"Future\"\nversion = \"0.1.0\"\napi_version = 2\ncapabilities = [\"totally.unknown\"]\n",
            "does not support",
        ),
    ] {
        let src = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(src.path(), manifest);
        let err = install::install(dir.to_str().unwrap(), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "got: {err}");
    }
}

/// Re-approval closes the stale-grant loop without a network fetch: the
/// disclosure is built from the installed manifest, and approving re-grants
/// pinned to its hash. A stale disclosure pin refuses rather than granting
/// something the user never saw.
#[tokio::test]
#[serial]
async fn reapprove_regrants_the_installed_manifest() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.regrant"
name = "Regrant"
version = "0.1.0"
api_version = 2
capabilities = ["net"]
"#,
    );
    install::install(dir.to_str().unwrap(), true).await.unwrap();
    assert!(load_registry().get("acme.regrant").unwrap().active());

    // The grant pins the manifest bytes: a comment-only edit revokes it.
    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.regrant")
        .join("aoe-plugin.toml");
    let mut text = std::fs::read_to_string(&installed).unwrap();
    text.push_str("\n# tampered\n");
    std::fs::write(&installed, &text).unwrap();
    assert!(load_registry()
        .get("acme.regrant")
        .unwrap()
        .needs_reapproval());

    // Grow the capability set on disk too; reload the process-global registry
    // the install API consults.
    let text = text.replace(
        "capabilities = [\"net\"]",
        "capabilities = [\"net\", \"notifications\"]",
    );
    std::fs::write(&installed, text).unwrap();
    agent_of_empires::plugin::reload_registry();
    assert!(load_registry()
        .get("acme.regrant")
        .unwrap()
        .needs_reapproval());

    let consent = install::reapprove_consent("acme.regrant").unwrap();
    assert_eq!(consent.capabilities, vec!["net", "notifications"]);

    // A pin that no longer matches the on-disk manifest refuses.
    let err = install::approve_installed("acme.regrant", "sha256:stale")
        .unwrap_err()
        .to_string();
    assert!(err.contains("changed on disk"), "got: {err}");

    install::approve_installed("acme.regrant", &consent.manifest_hash).unwrap();
    let reg = load_registry();
    let plugin = reg.get("acme.regrant").unwrap();
    assert!(plugin.active(), "re-approval must reactivate the plugin");
    assert!(!plugin.needs_reapproval());

    // A builtin is always granted; re-approval is meaningless for it. Only
    // checkable when a builtin is compiled in, and `aoe.web` needs `web`.
    #[cfg(feature = "web")]
    {
        let err = install::reapprove_consent("aoe.web")
            .unwrap_err()
            .to_string();
        assert!(err.contains("builtin"), "got: {err}");
    }
}

/// `approve_installed` re-reads the installed tree before honoring the pin: a
/// manifest changed on disk after the disclosure was built, while the
/// process-global registry is still stale, must refuse rather than write a
/// grant for content the user never saw.
#[tokio::test]
#[serial]
async fn approve_refuses_when_manifest_changes_after_disclosure() {
    let _home = isolate();
    let src = tempfile::tempdir().unwrap();
    let dir = write_plugin_dir(
        src.path(),
        r#"
id = "acme.stale"
name = "Stale"
version = "0.1.0"
api_version = 2
capabilities = ["net"]
"#,
    );
    install::install(dir.to_str().unwrap(), true).await.unwrap();

    // Stale the grant so there is a disclosure to approve.
    let installed = agent_of_empires::plugin::plugins_dir()
        .unwrap()
        .join("acme.stale")
        .join("aoe-plugin.toml");
    let text = std::fs::read_to_string(&installed).unwrap().replace(
        "capabilities = [\"net\"]",
        "capabilities = [\"net\", \"notifications\"]",
    );
    std::fs::write(&installed, text).unwrap();
    agent_of_empires::plugin::reload_registry();
    let consent = install::reapprove_consent("acme.stale").unwrap();

    // The manifest changes again after the disclosure, with no registry
    // reload in between (the popup sat open).
    let mut text = std::fs::read_to_string(&installed).unwrap();
    text.push_str("\n# tampered after disclosure\n");
    std::fs::write(&installed, text).unwrap();

    let err = install::approve_installed("acme.stale", &consent.manifest_hash)
        .unwrap_err()
        .to_string();
    assert!(err.contains("changed on disk"), "got: {err}");
    assert!(
        load_registry()
            .get("acme.stale")
            .unwrap()
            .needs_reapproval(),
        "the refused approval must leave the plugin inactive"
    );
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

/// An unverified GitHub install bails without --yes on a non-terminal stdin
/// rather than installing un-audited code: a bare repo with no release falls
/// back to the default branch (the releases API 404s), and an explicit `@ref`
/// is never verified.
#[tokio::test]
#[serial]
async fn unverified_github_sources_require_confirmation_without_yes() {
    let _home = isolate();
    for (repo, source) in [
        ("norel", "gh:acme/norel"),
        ("widget", "gh:acme/widget@main"),
    ] {
        let base = tempfile::tempdir().unwrap();
        let id = format!("acme.{repo}");
        make_bare_repo(
            base.path(),
            "acme",
            repo,
            &[(
                "aoe-plugin.toml",
                &format!(
                    "id = \"{id}\"\nname = \"{repo}\"\nversion = \"1.0.0\"\napi_version = 2\n"
                ),
            )],
        );
        let server = spawn_latest_release("other", "repo", "v9").await;

        let err = install::install(source, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unverified"), "{source}: {err}");
        assert!(load_registry().get(&id).is_none(), "{source}");

        server.abort();
    }
    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
    std::env::remove_var("AOE_UPDATE_API_BASE");
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

/// Only a vetted featured release lifts the reserved-namespace gate. The same
/// unvetted pin on a non-reserved id still installs, labelled by its source.
#[tokio::test]
#[serial]
async fn featured_pins_gate_the_reserved_namespace() {
    const UNVETTED: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    // (id, pin the installed tree, expected validation or refusal fragment)
    let cases: [(&str, bool, Result<&str, &str>); 3] = [
        ("agent-of-empires.official", true, Ok("featured")),
        ("acme.featured", false, Ok("local")),
        ("agent-of-empires.official", false, Err("reserved")),
    ];
    for (id, vetted, expected) in cases {
        let _home = isolate();
        let src = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            src.path(),
            &format!("id = \"{id}\"\nname = \"Pinned\"\nversion = \"1.0.0\"\napi_version = 2\n"),
        );
        let tree_hash = agent_of_empires::plugin::integrity::tree_hash(&dir).unwrap();
        // The installed tree is the second listed release, so an earlier vetted
        // release must not un-verify it.
        let current = if vetted { tree_hash.as_str() } else { UNVETTED };
        write_featured_versions(
            src.path(),
            id,
            dir.to_str().unwrap(),
            &[("0.9.0", UNVETTED), ("1.0.0", current)],
        );

        let result = install::install(dir.to_str().unwrap(), true).await;
        match expected {
            Ok(validation) => {
                let report = result.unwrap_or_else(|e| panic!("{id} vetted={vetted}: {e}"));
                assert_eq!(report.validation.as_str(), validation, "{id}");
                let reg = load_registry();
                assert_eq!(
                    reg.get(id).expect("installed").validation.as_str(),
                    validation
                );
                if validation == "featured" {
                    let lock = Lockfile::load().unwrap();
                    let locked = lock.get(id).unwrap();
                    assert_eq!(locked.trust, "featured");
                    assert_eq!(locked.tree_hash, tree_hash);
                }
            }
            Err(fragment) => {
                let err = result.expect_err("an unvetted reserved id must be refused");
                assert!(err.to_string().contains(fragment), "got: {err}");
                assert!(load_registry().get(id).is_none());
            }
        }
    }
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
/// is renamed away), so a build artifact lands at `<plugins_dir>/<id>`, and a
/// step whose `platforms` excludes the host OS is skipped. Uses a bare `sh`
/// launch command (resolves on PATH) so install's post-build entrypoint check
/// passes without the build having to produce an executable.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn build_steps_run_in_final_dir_for_matching_platforms() {
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

[[runtime.build]]
command = ["cp", "aoe-plugin.toml", "should-not-exist"]
platforms = ["windows"]
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
    let before = update_check::outdated().await;
    let s = before.iter().find(|s| s.id == "acme.upd").expect("present");
    assert!(
        !s.needs_update && s.error.is_none(),
        "fresh install is current: {s:?}"
    );

    // A clean (no consent change) newer version on the remote.
    push_new_commit(base.path(), "acme", "upd", &[("aoe-plugin.toml", &v2)]);
    let after = update_check::outdated().await;
    let s = after.iter().find(|s| s.id == "acme.upd").expect("present");
    assert!(s.needs_update, "new commit detected: {s:?}");
    let rec = std::sync::Arc::new(RecordingNotifier::default());
    let notifier: std::sync::Arc<dyn auto_update::UpdateNotifier> = rec.clone();
    let summary = auto_update::sweep(Some(&notifier)).await;
    assert_eq!(summary.applied, vec!["acme.upd".to_string()], "{summary:?}");
    assert_eq!(
        rec.applied.lock().unwrap().as_slice(),
        ["acme.upd".to_string()],
        "the host restarts the updated plugin's worker",
    );
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "2.0.0",
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

/// The manifest a capability-expanding update fetches.
fn with_net_cap_v2() -> String {
    PLAIN_MANIFEST.replace("1.0.0", "2.0.0").replace(
        "api_version = 2",
        "api_version = 2\ncapabilities = [\"net\"]",
    )
}

/// Install plain acme.upd from a local bare repo tracking `@main`, then push a
/// v2 that adds the `net` capability. Returns the temp base so the caller keeps
/// the repo alive.
async fn install_then_push_cap_update() -> TempDir {
    let base = tempfile::tempdir().unwrap();
    make_bare_repo(
        base.path(),
        "acme",
        "upd",
        &[("aoe-plugin.toml", PLAIN_MANIFEST)],
    );
    install::install("gh:acme/upd@main", true).await.unwrap();
    push_new_commit(
        base.path(),
        "acme",
        "upd",
        &[("aoe-plugin.toml", &with_net_cap_v2())],
    );
    base
}

#[tokio::test]
#[serial]
async fn preview_reports_consent_required_for_capability_change() {
    let _home = isolate();
    let _base = install_then_push_cap_update().await;

    match install::preview_update("acme.upd").await.unwrap() {
        UpdatePreview::ConsentRequired { consent, dismissed } => {
            assert_eq!(consent.from_version, "1.0.0");
            assert_eq!(consent.to_version, "2.0.0");
            assert_eq!(consent.added_capabilities, vec!["net".to_string()]);
            assert!(consent.removed_capabilities.is_empty());
            assert!(!dismissed, "a fresh update is not pre-dismissed");
            assert!(!consent.fingerprint.is_empty());
        }
        other => panic!("expected consent_required, got {other:?}"),
    }

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn apply_update_grants_the_new_capability_set() {
    let _home = isolate();
    let _base = install_then_push_cap_update().await;

    let fingerprint = match install::preview_update("acme.upd").await.unwrap() {
        UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
        other => panic!("expected consent_required, got {other:?}"),
    };

    install::apply_update(
        "acme.upd",
        Some(fingerprint),
        &install::OperationLog::Inherit,
    )
    .await
    .unwrap();

    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "2.0.0",
    );
    let reg = load_registry();
    let plugin = reg.get("acme.upd").expect("present");
    assert!(plugin.active(), "the approved update is granted and active");

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn apply_update_rejects_a_stale_fingerprint() {
    let _home = isolate();
    let _base = install_then_push_cap_update().await;

    // The user approved a different (stale) fingerprint than what is now fetched:
    // the apply must refuse rather than grant something never disclosed.
    let err = install::apply_update(
        "acme.upd",
        Some("sha256:stale||community".to_string()),
        &install::OperationLog::Inherit,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("changed since it was shown"), "got: {err}");
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "1.0.0",
        "a rejected apply keeps the prior version",
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[tokio::test]
#[serial]
async fn declining_keeps_the_prior_version_and_stops_nagging() {
    let _home = isolate();
    let _base = install_then_push_cap_update().await;

    let fingerprint = match install::preview_update("acme.upd").await.unwrap() {
        UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
        other => panic!("expected consent_required, got {other:?}"),
    };

    // Decline: record the dismissal. The new version is never applied.
    install::dismiss_update("acme.upd", &fingerprint).unwrap();
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "1.0.0",
        "the previously trusted version stays installed",
    );
    assert!(
        load_registry().get("acme.upd").unwrap().active(),
        "the prior version stays active",
    );

    // A re-preview now flags the dismissal so the surfaces stop re-prompting.
    match install::preview_update("acme.upd").await.unwrap() {
        UpdatePreview::ConsentRequired { dismissed, .. } => {
            assert!(dismissed, "the declined fingerprint is remembered");
        }
        other => panic!("expected consent_required, got {other:?}"),
    }

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}

#[derive(Default)]
struct RecordingNotifier {
    needs_approval: std::sync::Mutex<Vec<String>>,
    applied: std::sync::Mutex<Vec<String>>,
}

impl auto_update::UpdateNotifier for RecordingNotifier {
    fn needs_approval(&self, plugin_id: &str, _reason: &str) {
        self.needs_approval
            .lock()
            .unwrap()
            .push(plugin_id.to_string());
    }

    fn update_applied(
        self: std::sync::Arc<Self>,
        plugin_id: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move { self.applied.lock().unwrap().push(plugin_id) })
    }
}

#[tokio::test]
#[serial]
async fn sweep_notifies_then_respects_a_dismissal() {
    let _home = isolate();
    let _base = install_then_push_cap_update().await;

    let fingerprint = match install::preview_update("acme.upd").await.unwrap() {
        UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
        other => panic!("expected consent_required, got {other:?}"),
    };

    // First sweep: not dismissed, so the consent-needed skip notifies.
    let rec = std::sync::Arc::new(RecordingNotifier::default());
    let notifier: std::sync::Arc<dyn auto_update::UpdateNotifier> = rec.clone();
    auto_update::sweep(Some(&notifier)).await;
    assert_eq!(
        rec.needs_approval.lock().unwrap().as_slice(),
        ["acme.upd".to_string()],
        "an undismissed consent-needed skip notifies",
    );
    assert!(
        rec.applied.lock().unwrap().is_empty(),
        "a skipped update restarts nothing",
    );
    assert_eq!(
        Lockfile::load().unwrap().get("acme.upd").unwrap().version,
        "1.0.0",
        "a skipped update keeps the prior version",
    );

    // After dismissing this exact version, a later sweep stays silent.
    install::dismiss_update("acme.upd", &fingerprint).unwrap();
    let rec2 = std::sync::Arc::new(RecordingNotifier::default());
    let notifier2: std::sync::Arc<dyn auto_update::UpdateNotifier> = rec2.clone();
    auto_update::sweep(Some(&notifier2)).await;
    assert!(
        rec2.needs_approval.lock().unwrap().is_empty(),
        "a dismissed version does not re-notify",
    );
    assert!(
        rec2.applied.lock().unwrap().is_empty(),
        "a dismissed update restarts nothing",
    );

    std::env::remove_var("AOE_GITHUB_CLONE_BASE");
}
