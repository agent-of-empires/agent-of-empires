//! Helpers shared by the tests of more than one `instance` submodule.

use super::*;

/// Makes `Session::existence()` resolve to `Absent` whatever tmux server is up
/// (#2936). Keep the guard bound and mark the test serial: the cache is global.
#[must_use]
pub(super) fn force_session_absent() -> crate::tmux::SessionCacheGuard {
    let guard = crate::tmux::SessionCacheGuard::capture();
    guard.force_present(&["aoe_some_other_session"]);
    guard
}

/// Seeds the global `agent_detect_as` registry for one profile; the guard
/// restores the prior entries on drop.
pub(crate) fn install_aliases(
    profile: &str,
    aliases: &[(&str, &str)],
) -> crate::tmux::status_rules::ProfileRegistryGuard {
    let guard = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
    let mut config = crate::session::Config::default();
    for (agent, target) in aliases {
        config
            .session
            .agent_detect_as
            .insert(agent.to_string(), target.to_string());
    }
    crate::tmux::status_rules::install_from_config(profile, &config);
    guard
}

pub(crate) fn declare_execution_aliases(
    profile: &str,
    aliases: &[(&str, &str)],
    home: &std::path::Path,
) {
    let mut execution = toml::Table::new();
    let mut stores = toml::Table::new();
    for (alias, agent) in aliases {
        execution.insert((*alias).into(), toml::Value::String((*agent).into()));
        stores.insert(
            (*alias).into(),
            toml::Value::String(home.join(format!(".{agent}")).to_str().unwrap().into()),
        );
    }
    let session = toml::Table::from_iter([
        ("agent_execution_as".into(), toml::Value::Table(execution)),
        ("agent_config_dir".into(), toml::Value::Table(stores)),
    ]);
    let config = toml::Table::from_iter([("session".into(), toml::Value::Table(session))]);
    let path = crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
}

/// A path's identity for assertions: a symlinked temp or home root spells the
/// same directory two ways, so expectations compare the resolved identity.
pub(crate) fn path_identity(path: &std::path::Path) -> std::path::PathBuf {
    crate::session::capture::canonicalize_allowing_missing_leaf(path)
        .unwrap_or_else(|| path.to_path_buf())
}

pub(super) fn create_hermes_database(root: &std::path::Path) -> rusqlite::Connection {
    std::fs::create_dir_all(root).unwrap();
    let database = rusqlite::Connection::open(root.join("state.db")).unwrap();
    database.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY, parent_session_id TEXT, end_reason TEXT, ended_at REAL, model_config TEXT, source TEXT, started_at REAL, last_activity_at REAL, session_key TEXT, cwd TEXT); CREATE TABLE messages(session_id TEXT, timestamp REAL);").unwrap();
    database
}

pub(crate) fn publish_host_pi_transcript(
    instance_id: &str,
    sid: &str,
    root: &std::path::Path,
) -> PathBuf {
    let transcript = root
        .canonicalize()
        .unwrap()
        .join(format!("time_{sid}.jsonl"));
    std::fs::write(
        &transcript,
        format!("{{\"type\":\"session\",\"id\":\"{sid}\"}}\n"),
    )
    .unwrap();
    crate::hooks::write_session_id_via_guard(instance_id, sid, None).unwrap();
    let directory = crate::hooks::ensure_instance_dir_path(instance_id).unwrap();
    std::fs::write(directory.join("session_path"), transcript.to_str().unwrap()).unwrap();
    transcript
}

pub(super) fn install_container_transport(
    root: &std::path::Path,
    name: &str,
    volumes: &[crate::containers::VolumeMount],
) -> crate::session::test_support::EnvGuard {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("transport");
    let native_bin = root.join("native-bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&native_bin).unwrap();
    let program = native_bin.join("prime-agent");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut mounts = volumes
        .iter()
        .map(|mount| {
            serde_json::json!({
                "Type": "bind", "Source": mount.host_path,
                "Destination": mount.container_path, "RW": !mount.read_only,
            })
        })
        .collect::<Vec<_>>();
    mounts.push(serde_json::json!({"Type": "bind", "Source": native_bin,
        "Destination": "/usr/local/bin", "RW": true}));
    let fixture = serde_json::json!({"name": name, "mounts": mounts});
    let script = r#"
const fs = require("node:fs"), path = require("node:path").posix;
const mounts = fixture.mounts.toSorted((a, b) => b.Destination.length - a.Destination.length);
function physical(p) {
  const m = mounts.find(m => p === m.Destination || p.startsWith(m.Destination + "/"));
  if (m) return m.Source + p.slice(m.Destination.length);
  return /^\/(usr|bin|lib|lib64)(\/|$)/.test(p) ? p : null;
}
function canonical(p, depth = 0) {
  if (depth > 40) throw Error("symlink loop");
  const parts = path.normalize(p).split("/").filter(Boolean);
  for (let i = 0; i < parts.length; i++) {
    const prefix = "/" + parts.slice(0, i + 1).join("/"), host = physical(prefix);
    if (host && fs.lstatSync(host, {throwIfNoEntry: false})?.isSymbolicLink()) {
      const target = fs.readlinkSync(host);
      return canonical(path.resolve(path.dirname(prefix), target, ...parts.slice(i + 1)), depth + 1);
    }
  }
  return "/" + parts.join("/");
}
let args = process.argv.slice(2);
if (args[0] === "container" && args[1] === "inspect" && args.at(-1) === fixture.name) {
  process.stdout.write(JSON.stringify({id: "fixture:" + fixture.name, mounts: fixture.mounts}) + "\n");
} else if (args[0] === "inspect" || (args[0] === "container" && args[1] === "inspect")) {
  process.exit(1);
} else if (args.shift() === "exec") {
  let cwd = "/";
  while (args[0]?.startsWith("-")) {
    if (args[0] === "-w") { args.shift(); cwd = args.shift(); }
    else throw Error("unexpected exec option: " + args[0]);
  }
  if (args.shift() !== "fixture:" + fixture.name) throw Error("unknown container generation");
  if (args.join(" ") === "env -0") {
    process.stdout.write("HOME=/root\0PATH=/usr/local/bin:/usr/bin:/bin\0");
  } else if (args[0] === "/bin/sh" && args[1] === "-c" && args[3] === "aoe-path") {
    process.stdout.write(canonical(args[4]) + "\n");
  } else if (args[0] === "/bin/sh" && args[1] === "-c" && args[3] === "aoe-native-program") {
    const command = args[5];
    const candidates = command.includes("/") ? [path.resolve(cwd, command)]
      : args[4].split(":").map(dir => path.resolve(cwd, dir, command));
    const found = candidates.find(p => {
      const host = physical(canonical(p));
      try { return host && fs.statSync(host).isFile() && (fs.statSync(host).mode & 0o111); }
      catch { return false; }
    });
    if (!found) process.exit(1);
    process.stdout.write(found + "\n");
  } else throw Error("unexpected native probe: " + JSON.stringify(args));
} else throw Error("unexpected transport operation: " + JSON.stringify(args));
"#;
    let runtime = bin.join("docker");
    let node = which::which("node").unwrap();
    std::fs::write(
        &runtime,
        format!("#!{}\nconst fixture = {fixture};\n{script}", node.display()),
    )
    .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config_path = crate::session::get_app_dir().unwrap().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path)
        .unwrap_or_default()
        .parse::<toml::Table>()
        .unwrap();
    config
        .entry("sandbox".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap()
        .insert(
            "container_runtime".into(),
            toml::Value::String("docker".into()),
        );
    std::fs::write(config_path, toml::to_string(&config).unwrap()).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    crate::session::test_support::EnvGuard::set(&[
        (
            "PATH",
            std::path::PathBuf::from(std::env::join_paths(paths).unwrap()),
        ),
        (
            "DOCKER_HOST",
            std::path::PathBuf::from(format!("unix://{}/docker.sock", root.display())),
        ),
        ("DOCKER_CONTEXT", std::path::PathBuf::new()),
        ("XDG_RUNTIME_DIR", root.to_path_buf()),
    ])
}

pub(super) fn write_sidecar(instance_id: &str, sid: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let base = crate::hooks::hook_base_path();
    if !base.exists() {
        std::fs::create_dir_all(&base).expect("create hook base dir");
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
        .expect("set hook base mode 0700");
    let dir = crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("set hook instance mode 0700");
    std::fs::write(dir.join("session_id"), sid).unwrap();
    dir
}

pub(super) fn seed_disk_for_sidecar_test(profile: &str, inst: &Instance) {
    let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
    let snapshot = inst.clone();
    storage
        .update(|i, g| {
            *i = vec![snapshot.clone()];
            *g = crate::session::GroupTree::new_with_groups(std::slice::from_ref(&snapshot), &[])
                .get_all_groups();
            Ok(())
        })
        .unwrap();
}

pub(super) const SIDECAR_TEST_FRESH_UUID: &str = "11111111-2222-4333-8444-555555555555";

pub(super) fn test_sandbox(name: &str, workdir: Option<&str>) -> SandboxInfo {
    SandboxInfo {
        enabled: true,
        container_id: None,
        image: "test-image".to_string(),
        container_name: name.to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: workdir.map(str::to_string),
    }
}

pub(super) fn tool_instance(tool: &str, path: &str) -> Instance {
    let mut inst = Instance::new(tool, path);
    inst.tool = tool.to_string();
    inst
}
/// Mirror launch admission for hand-built sandbox fixtures without holding the
/// transition lock across the behavior under test.
pub(super) fn admit_sandbox_fixture(inst: &Instance) {
    let app = crate::session::get_app_dir().unwrap();
    for root in crate::migrations::v033_isolate_sandbox_content::instance_roots(inst).unwrap() {
        std::fs::create_dir_all(&root.path).unwrap();
        let roles: Vec<&str> = root.roles.iter().map(String::as_str).collect();
        crate::migrations::v033_isolate_sandbox_content::certify_test_content(
            &app, &inst.id, &root.path, &roles,
        )
        .unwrap();
    }
}
