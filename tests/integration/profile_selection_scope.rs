//! An exported-but-empty `AGENT_OF_EMPIRES_PROFILE` is one statement for a
//! read and a different one for a write. For the seven served reads an empty
//! value selects the configured default profile, because that is what the local
//! path does with an empty profile name. It is not "the user named no
//! profile": `aoe project add` reads the same field to decide whether the row
//! lands in the profile registry or the global one, and `aoe ps` reads it to
//! decide whether it looks at one profile or all of them. Both have always
//! treated an empty variable as a selection that resolves to the default, and
//! this pins that: the read rule must not reach the write path.

use agent_of_empires::session::{
    create_profile, set_default_profile, Instance, Storage, APP_DIR_NAME_XDG,
};
use serde_json::Value;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::{setup_temp_home, tmux_socket};

/// Run the real binary against `home`. `profile` is what the user exported,
/// including the empty string: the whole subject of these tests is a variable
/// that is set and has no value, so it is never "absent" here by accident.
fn aoe(home: &Path, tmux_socket: &Path, profile: &str, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("AOE_TMUX_SOCKET", tmux_socket)
        .env("AGENT_OF_EMPIRES_PROFILE", profile)
        .env_remove("AOE_DAEMON_URL")
        .env_remove("AOE_DAEMON_TOKEN")
        .args(args)
        .output()
        .expect("the aoe binary runs");
    assert!(
        output.status.success(),
        "`aoe {}` with AGENT_OF_EMPIRES_PROFILE={profile:?} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is utf-8")
}

fn app_dir(home: &Path) -> PathBuf {
    home.join(".config").join(APP_DIR_NAME_XDG)
}

/// The rows in one scope's registry, or an empty inventory for a registry file
/// that was never written: which is the other half of the assertion.
fn registered(home: &Path, profile: Option<&str>) -> Vec<Value> {
    let path = match profile {
        Some(profile) => app_dir(home)
            .join("profiles")
            .join(profile)
            .join("projects.json"),
        None => app_dir(home).join("projects.json"),
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("{} is not a project registry: {e}", path.display())),
        Err(_) => Vec::new(),
    }
}

/// An empty profile variable writes to main and reads the configured default.
/// The registry identifies the profile chosen by each operation.
#[test]
#[serial]
fn an_empty_profile_variable_writes_where_it_wrote_before_and_reads_as_the_default() {
    let home = setup_temp_home();
    let home = home.path().to_path_buf();
    let socket = tmux_socket();

    create_profile("main").expect("main");
    create_profile("other").expect("other");
    set_default_profile("main").expect("default");

    // A row registered in `other`, so the control below proves the row is
    // there at all when that profile is named.
    let id = "psscope0001";
    let title = "Scope";
    let mut instance = Instance::new(title, &home.to_string_lossy());
    instance.id = id.to_string();
    instance.title = title.to_string();
    let sessions = vec![instance];
    Storage::new_unwatched("other")
        .expect("other storage")
        .update(|i, g| {
            *i = sessions;
            *g = Vec::new();
            Ok(())
        })
        .expect("seed other");

    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).expect("the directory to register");
    let canonical = repo.canonicalize().expect("canonical repo path");

    let added = aoe(
        &home,
        &socket,
        "",
        &["project", "add", repo.to_str().expect("utf-8 path")],
    );
    assert!(
        added.contains("repo"),
        "`aoe project add` says nothing about the row it added: {added}"
    );

    // The write landed in the profile registry of the profile the empty value
    // resolved to, and the global registry is still empty.
    let stored = registered(&home, Some("main"));
    assert_eq!(
        stored
            .iter()
            .map(|row| (row["name"].as_str(), row["path"].as_str()))
            .collect::<Vec<_>>(),
        vec![(Some("repo"), Some(canonical.to_string_lossy().as_ref()),)],
        "an empty AGENT_OF_EMPIRES_PROFILE must add to the default profile's registry"
    );
    assert!(
        registered(&home, None).is_empty(),
        "an empty AGENT_OF_EMPIRES_PROFILE must not add to the global registry"
    );

    // Empty profile selections read the configured default.
    let listed: Vec<Value> =
        serde_json::from_str(&aoe(&home, &socket, "", &["project", "list", "--json"]))
            .expect("project list --json");
    assert_eq!(
        serde_json::to_value(&listed).expect("rows"),
        serde_json::json!([{"name": "repo", "path": canonical.to_string_lossy(), "scope": "profile"}]),
        "an empty value must answer from the configured default profile"
    );
    let other: Vec<Value> = serde_json::from_str(&aoe(
        &home,
        &socket,
        "other",
        &["project", "list", "--json"],
    ))
    .expect("project list --json for the other profile");
    assert!(
        other.is_empty(),
        "the other profile's registry is empty and must answer empty: {other:?}"
    );

    // The read half is scoped by the same selection, and is read from the
    // registry rather than from the substrate. `aoe ps` only lists rows a live
    // tmux session provides, which makes the assertion depend on the runner's
    // tmux being able to create and expose a session; the registry is what
    // the selection actually chooses between, and it is deterministic.
    let scoped: Vec<Value> =
        serde_json::from_str(&aoe(&home, &socket, "", &["list", "--json"])).expect("list --json");
    assert!(
        !scoped.iter().any(|row| row["id"] == id),
        "an empty AGENT_OF_EMPIRES_PROFILE must read one profile's sessions: {scoped:?}"
    );
    // The control: the same row is there when the profile is named, so the
    // assertion above is about the scope and not about a row nobody can see.
    let named: Vec<Value> =
        serde_json::from_str(&aoe(&home, &socket, "", &["-p", "other", "list", "--json"]))
            .expect("list --json for the other profile");
    assert!(
        named.iter().any(|row| row["id"] == id),
        "the `other` profile's session must be visible when that profile is named: {named:?}"
    );
}
