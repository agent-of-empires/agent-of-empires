use aoe_plugin_api::{
    lucide_icon_name_ok, screenshot_path_ok, ManifestError, PluginManifest, RuntimeSpec,
    SettingType, UiSlot,
};

fn manifest(api_version: u32, body: &str) -> String {
    format!(
        "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"0.1.0\"\napi_version = {api_version}\n{body}"
    )
}

fn parse(api_version: u32, body: &str) -> PluginManifest {
    PluginManifest::from_toml_str(&manifest(api_version, body))
        .unwrap_or_else(|e| panic!("manifest should parse: {e}\n{body}"))
}

fn invalid(api_version: u32, body: &str) -> Vec<String> {
    match PluginManifest::from_toml_str(&manifest(api_version, body)) {
        Err(ManifestError::Invalid(m)) => m,
        other => panic!("expected Invalid for {body:?}, got {other:?}"),
    }
}

#[test]
fn minimal_manifest_parses_and_round_trips() {
    let m = parse(1, "description = \"d\"");
    assert_eq!(m.id.as_str(), "acme.thing");
    assert_eq!(m.name, "Thing");
    assert_eq!(m.version, "0.1.0");
    assert_eq!(m.api_version, 1);
    let reparsed =
        PluginManifest::from_toml_str(&toml::to_string(&m).unwrap()).expect("round-trips");
    assert_eq!(reparsed.id.as_str(), "acme.thing");
    assert!(parse(2, "").description.is_empty());
}

#[test]
fn agent_declarations_require_api_21() {
    let declarations = [
        "[agent_integrations.launch]\noneshot_flag = \"-p\"\n",
        "[agent_integrations.detection]\nmode = \"native_only\"\n",
        "[agent_integrations.reply_fallback]\nallow = []\ndeny = []\n",
    ];
    for declaration in declarations {
        let body = format!(
            "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n{declaration}"
        );
        let errors = invalid(20, &body);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("api_version >= 21")),
            "{errors:?}"
        );
    }
    parse(
        21,
        "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n[agent_integrations.launch]\noneshot_flag = \"-p\"\n",
    );
    parse(
        20,
        "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n",
    );
}

#[test]
fn agent_contract_rejects_unsafe_or_unsupported_declarations() {
    let valid = r#"
capabilities = ["session.integration"]
[[agent_integrations]]
tool = "example"
command = "example"
provider_versions = ["^1"]
[agent_integrations.launch]
oneshot_flag = "-p"
instruction_flag = "--instruction {}"
[agent_integrations.launch.container_env]
EXAMPLE_HOME = "/state"
[agent_integrations.launch.resume]
kind = "flag"
flag = "--resume"
[agent_integrations.launch.capture]
backend = "omp"
host = "pane_scoped"
sandbox = "pane_scoped"
[agent_integrations.detection]
mode = "native_only"
[agent_integrations.reply_fallback]
allow = [{ kind = "named", value = "Enter" }]
deny = [{ kind = "literal", value = "n" }]
"#;
    let check = |body: &str| {
        let parsed: PluginManifest = toml::from_str(&manifest(21, body)).unwrap();
        match parsed.validate() {
            Err(ManifestError::Invalid(errors)) => errors,
            result => panic!("expected invalid declaration: {result:?}"),
        }
    };
    parse(21, valid);
    for body in [
        valid.replace("host = \"pane_scoped\"", "host = \"unsupported\""),
        valid.replace("sandbox = \"pane_scoped\"", "sandbox = \"unsupported\""),
    ] {
        parse(21, &body);
    }
    for (body, expected) in [
        (
            valid.replace("--instruction {}", "--instruction '{}'"),
            "instruction_flag",
        ),
        (
            valid.replace("--instruction {}", "--instruction {}suffix"),
            "instruction_flag",
        ),
        (
            valid.replace("oneshot_flag = \"-p\"", "oneshot_flag = \"-p\u{202e}\""),
            "oneshot_flag",
        ),
        (
            valid.replace("--instruction {}", "--instruction\u{200b} {}"),
            "instruction_flag",
        ),
        (
            valid.replace("flag = \"--resume\"", "flag = \"--resume\u{200b}\""),
            "launch.resume",
        ),
        (
            valid.replace("value = \"n\"", "value = \"n\u{202e}\""),
            "invalid key",
        ),
        (valid.replace("EXAMPLE_HOME", "HOME"), "container_env"),
        (
            valid.replace(
                "[agent_integrations.launch.capture]\nbackend = \"omp\"\nhost = \"pane_scoped\"\nsandbox = \"pane_scoped\"\n",
                "",
            ),
            "resume requires OMP capture",
        ),
        (
            valid.replace(
                "host = \"pane_scoped\"\nsandbox = \"pane_scoped\"",
                "host = \"unsupported\"\nsandbox = \"unsupported\"",
            ),
            "resume requires OMP capture",
        ),
        (
            valid.replace("backend = \"omp\"", "backend = \"codex\""),
            "capture combination",
        ),
        (
            valid.replace(
                "sandbox = \"pane_scoped\"",
                "sandbox = \"managed_exclusive_store\"",
            ),
            "capture combination",
        ),
        (
            valid.replace("value = \"Enter\"", "value = \"F01\""),
            "invalid key",
        ),
    ] {
        let errors = check(&body);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{expected}: {errors:?}"
        );
    }
}

#[test]
fn agent_integrations_require_api_capability_and_unique_valid_tools() {
    let base = r#"
id = "acme.session-agent"
name = "Session Agent"
version = "0.1.0"
api_version = {api_version}
capabilities = {capabilities}
[[agent_integrations]]
tool = "session-agent"
provider_versions = ["=1.2.3"]
command = "session-agent --"
replies = true
"#;
    let valid = base
        .replace("{api_version}", "19")
        .replace("{capabilities}", r#"["session.integration"]"#);
    let parsed = PluginManifest::from_toml_str(&valid).expect("valid integration parses");
    assert_eq!(
        parsed.agent_integrations[0].command.as_deref(),
        Some("session-agent --")
    );
    assert_eq!(parsed.agent_integrations[0].tool, "session-agent");
    let launch_only = base
        .replace("{api_version}", "19")
        .replace("{capabilities}", "[]")
        .replace("provider_versions = [\"=1.2.3\"]\n", "")
        .replace("replies = true\n", "replies = false\n");
    let parsed_launch_only =
        PluginManifest::from_toml_str(&launch_only).expect("launch-only agent parses");
    assert!(parsed_launch_only.agent_integrations[0]
        .provider_versions
        .is_empty());
    for version in ["1.2.2", "1.2.4", "1.2.3-rc.1", "2.0.0", "unknown"] {
        assert!(parsed.agent_integrations[0]
            .provider_compat(version)
            .is_err());
    }
    assert!(parsed.agent_integrations[0]
        .provider_compat("1.2.3")
        .is_ok());
    for ranges in ["[]", r#"["not semver"]"#, r#"[""]"#] {
        let invalid = valid.replace(r#"["=1.2.3"]"#, ranges);
        assert!(matches!(
            PluginManifest::from_toml_str(&invalid),
            Err(ManifestError::Invalid(_))
        ));
    }

    let cases = [
        (
            base.replace("{api_version}", "18")
                .replace("{capabilities}", r#"["session.integration"]"#),
            "require api_version >= 19",
        ),
        (
            base.replace("{api_version}", "19")
                .replace("{capabilities}", "[]"),
            "require the session.integration capability",
        ),
        (
            base.replace("{api_version}", "19")
                .replace("{capabilities}", r#"["session.integration"]"#)
                .replace("tool = \"session-agent\"", "tool = \"Session-Agent\""),
            "lowercase agent id",
        ),
        (
            format!("{valid}\n[[agent_integrations]]\ntool = \"session-agent\"\nprovider_versions = [\"=1.2.3\"]\n"),
            "must not repeat tool",
        ),
        (
            base.replace("{api_version}", "19")
                .replace("{capabilities}", r#"["session.integration"]"#)
                .replace("tool = \"session-agent\"", "tool = \"\""),
            "must be a nonempty lowercase agent id",
        ),
        (
            launch_only.replace("replies = false", "replies = true"),
            "require the session.integration capability",
        ),
        (
            base.replace("{api_version}", "18")
                .replace("{capabilities}", r#"["session.integration"]"#)
                .replace(
                    "\n[[agent_integrations]]\ntool = \"session-agent\"\nprovider_versions = [\"=1.2.3\"]\ncommand = \"session-agent --\"\nreplies = true\n",
                    "\n",
                ),
            "session.integration capability requires api_version >= 19",
        ),
    ];
    for (toml, expected_error) in cases {
        let error = PluginManifest::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(&error, ManifestError::Invalid(errors) if errors.iter().any(|item| item.contains(expected_error))),
            "expected {expected_error:?}, got {error:?}"
        );
    }
}

#[test]
fn agent_launch_commands_use_a_literal_prefix_grammar() {
    let manifest = |command: &str| {
        format!(
            "id = \"acme.agent\"\nname = \"Agent\"\nversion = \"0.1.0\"\napi_version = 19\n\
             [[agent_integrations]]\ntool = \"session-agent\"\ncommand = {command:?}\n"
        )
    };
    // Only the prefix is constrained; agent arguments stay free-form.
    for command in [
        "session-agent",
        "PATH=/opt/agents/bin session-agent --flag",
        "TOKEN=local OTHER=2 session-agent --filter='a b' \"$(not run)\"",
    ] {
        PluginManifest::from_toml_str(&manifest(command))
            .unwrap_or_else(|error| panic!("{command:?} should be accepted: {error}"));
    }
    // Shell syntax in the prefix disagrees with discovery, so it fails install.
    for command in [
        "PATH=$HOME/bin session-agent",
        "PATH=\"$HOME/bin\" session-agent",
        "PATH=${HOME}/bin session-agent",
        "PATH=${HOME/bin session-agent",
        "PATH=~/bin session-agent",
        "~/bin/session-agent",
        "PATH=/opt/agents\\\n/bin session-agent",
        "\"session-agent",
        "\"\" session-agent",
        "PATH=$(printf /tmp) session-agent",
        "PATH=`echo /tmp` session-agent",
        "9INVALID=1 session-agent",
        "-i session-agent",
        // A newline ends the shell command before `env` gets an executable.
        "TOKEN=local\nsession-agent",
        // Separators and globs rewrite the words `env` receives.
        "TOKEN=local; session-agent",
        "TOKEN=local|session-agent",
        "PATH=/opt/*/bin session-agent",
    ] {
        let error = PluginManifest::from_toml_str(&manifest(command)).unwrap_err();
        assert!(
            matches!(&error, ManifestError::Invalid(errors) if errors.iter().any(|item| item.contains("literal executable"))),
            "{command:?} should be rejected, got {error:?}"
        );
    }
}

#[test]
fn contribution_sections_parse() {
    let m = parse(
        4,
        r#"capabilities = ["session.read", "net", "some.future.cap"]
aoe_version = ">=0.10, <0.12"

[[commands]]
id = "do-thing"
title = "Do Thing"

[[keybinds]]
command = "plugin.acme.kit.do-thing"
key = "Ctrl+K"

[[settings]]
key = "endpoint"
label = "Endpoint"

[[settings]]
key = "enabled"
type = "boolean"
default = true

[[settings]]
key = "retries"
type = "integer"
min = 0
max = 10
default = 3
advanced = true

[[settings]]
key = "mode"
type = "select"
options = ["fast", "slow"]
default = "fast"

[setting_defaults]
"theme.idle_decay_minutes" = 10

[[themes]]
name = "kit-dark"
path = "themes/dark.toml"

[[ui]]
slot = "status-bar"
id = "panel"

[[status]]
id = "queue"
label = "Queue depth"

[[status]]
id = "nolabel"
"#,
    );
    assert!(m.capabilities[0].is_known());
    assert!(!m.capabilities[2].is_known());
    assert_eq!(m.commands.len(), 1);
    assert_eq!(m.keybinds[0].key, "Ctrl+K");
    assert_eq!(m.settings[0].value_type, SettingType::String);
    assert_eq!(m.settings[1].value_type, SettingType::Bool);
    assert_eq!(m.settings[2].min, Some(0));
    assert!(m.settings[2].advanced);
    assert_eq!(m.settings[3].options, ["fast", "slow"]);
    assert_eq!(
        m.setting_defaults.get("theme.idle_decay_minutes"),
        Some(&toml::Value::Integer(10))
    );
    assert_eq!(m.themes[0].path, "themes/dark.toml");
    assert_eq!(m.ui[0].slot, UiSlot::StatusBar);
    assert_eq!(m.status[0].label, "Queue depth");
    assert_eq!(m.status[1].label, "");

    assert!(m.host_compat("0.11.0").is_ok());
    assert!(m.host_compat("0.11.1-nightly.20261108").is_ok());
    assert!(m.host_compat("0.13.0-nightly.20261108").is_err());
    assert!(m.host_compat("0.10.0-rc.1").is_err());
    assert!(m.host_compat("0.11.0-rc.1").is_err());
    assert!(m.host_compat("0.12.0-nightly.20261108").is_err());
    assert!(m.host_compat("0.12.1-nightly.20261108").is_err());
    let err = m.host_compat("0.13.0").unwrap_err();
    assert!(
        err.contains("plugin requires aoe") && err.contains("0.13.0"),
        "{err}"
    );
    assert!(parse(4, "").host_compat("99.0.0").is_ok());
    // A nightly is the stable it builds on, not the unreleased next patch.
    let next = parse(4, "aoe_version = \">=0.11.1\"\n");
    assert!(next.host_compat("0.11.1-nightly.20261108").is_err());
    assert!(next.host_compat("0.11.2-nightly.20261108").is_ok());
}

#[test]
fn runtime_requires_worker_capability() {
    let runtimes = [
        "[runtime]\nkind = \"command\"\ncommand = [\"./worker\"]\n",
        "[runtime]\nkind = \"command\"\nsystem = true\ncommand = [\"worker\"]\n",
        "[runtime]\nkind = \"release-binary\"\nasset = \"worker-${target}.tar.gz\"\n",
    ];
    for version in [2, aoe_plugin_api::API_VERSION] {
        for runtime in runtimes {
            for capabilities in ["", "capabilities = []\n", "capabilities = [\"net\"]\n"] {
                let errors = invalid(version, &format!("{capabilities}{runtime}"));
                assert!(
                    errors.iter().any(|error| error.contains("runtime.worker")),
                    "version {version}, {capabilities}{runtime}: {errors:?}"
                );
            }
            parse(
                version,
                &format!("capabilities = [\"runtime.worker\"]\n{runtime}"),
            );
        }
    }
    parse(2, "capabilities = [\"runtime.worker\"]\n");
}

#[test]
fn send_keys_enter_delay_requires_api_22_and_caps_at_two_seconds() {
    let declaration = "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n\
                       [agent_integrations.launch]\n\
                       send_keys_enter_delay_ms = 150\n";

    let parsed = parse(22, declaration);
    assert_eq!(
        parsed.agent_integrations[0]
            .launch
            .as_ref()
            .and_then(|launch| launch.send_keys_enter_delay_ms),
        Some(150)
    );
    assert_eq!(
        PluginManifest::from_toml_str(&toml::to_string(&parsed).unwrap())
            .expect("round-trips")
            .agent_integrations[0]
            .launch
            .as_ref()
            .and_then(|launch| launch.send_keys_enter_delay_ms),
        Some(150)
    );

    let errors = invalid(21, declaration);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("send_keys_enter_delay_ms requires api_version >= 22")),
        "{errors:?}"
    );

    let errors = invalid(22, &declaration.replace("150", "2001"));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("send_keys_enter_delay_ms must be at most 2000")),
        "{errors:?}"
    );

    parse(22, &declaration.replace("150", "2000"));
}

#[test]
fn session_id_capture_requires_api_22_and_a_worker_runtime() {
    let runtime = "capabilities = [\"runtime.worker\"]\n\
                   [runtime]\n\
                   kind = \"command\"\n\
                   command = [\".venv/bin/worker\"]\n";
    let declaration = "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n\
                       [agent_integrations.session_id]\n\
                       method = \"example.session_id\"\n";
    let body = format!("{runtime}{declaration}");

    let parsed = parse(22, &body);
    let session_id = parsed.agent_integrations[0]
        .session_id
        .as_ref()
        .expect("declared");
    assert_eq!(session_id.timeout_ms, 5_000);
    assert!(session_id.launch_method.is_none());
    assert_eq!(
        PluginManifest::from_toml_str(&toml::to_string(&parsed).unwrap())
            .expect("round-trips")
            .agent_integrations[0]
            .session_id,
        parsed.agent_integrations[0].session_id
    );

    let errors = invalid(21, &body);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("session_id requires api_version >= 22")),
        "{errors:?}"
    );

    let errors = invalid(22, declaration);
    assert!(
        errors
            .iter()
            .any(|error| error
                .contains("session_id requires the plugin to declare a [runtime] worker")),
        "{errors:?}"
    );
}

#[test]
fn session_id_rejects_invalid_methods_and_timeouts() {
    let runtime = "capabilities = [\"runtime.worker\"]\n\
                   [runtime]\n\
                   kind = \"command\"\n\
                   command = [\".venv/bin/worker\"]\n";
    let body = |session_id: &str| {
        format!(
            "{runtime}[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n\
             [agent_integrations.session_id]\n{session_id}"
        )
    };

    for method in [
        "\"\"",
        "\"example session\"",
        "\"1example\"",
        "\"example..id\"",
    ] {
        let errors = invalid(22, &body(&format!("method = {method}\n")));
        assert!(
            errors
                .iter()
                .any(|error| error.contains("session_id.method must be a worker RPC method name")),
            "{method}: {errors:?}"
        );
    }
    let errors = invalid(
        22,
        &body("method = \"example.session_id\"\nlaunch_method = \"example launch\"\n"),
    );
    assert!(
        errors.iter().any(
            |error| error.contains("session_id.launch_method must be a worker RPC method name")
        ),
        "{errors:?}"
    );
    for timeout in [0, 249, 30_001] {
        let errors = invalid(
            22,
            &body(&format!(
                "method = \"example.session_id\"\ntimeout_ms = {timeout}\n"
            )),
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("session_id.timeout_ms must be")),
            "{timeout}: {errors:?}"
        );
    }
    parse(
        22,
        &body("method = \"example.session_id\"\nlaunch_method = \"example.session_id.launch\"\ntimeout_ms = 1000\n"),
    );
}

#[test]
fn runtime_specs_parse() {
    let m = parse(
        2,
        r#"capabilities = ["runtime.worker"]
[runtime]
kind = "command"
command = [".venv/bin/worker"]

[[runtime.build]]
command = ["python3", "-m", "venv", ".venv"]

[[runtime.build]]
command = [".venv/bin/pip", "install", "."]
platforms = ["linux", "macos"]
"#,
    );
    let Some(RuntimeSpec::Command {
        command,
        system,
        build,
    }) = m.runtime
    else {
        panic!("expected command runtime");
    };
    assert_eq!(command, [".venv/bin/worker"]);
    assert!(!system);
    assert!(build[0].platforms.is_empty());
    assert_eq!(build[1].command, [".venv/bin/pip", "install", "."]);
    assert_eq!(build[1].platforms, ["linux", "macos"]);

    let m = parse(
        2,
        "capabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"release-binary\"\nasset = \"thing-${target}.tar.gz\"\nbin = \"thing\"\n",
    );
    let Some(RuntimeSpec::ReleaseBinary { asset, bin }) = m.runtime else {
        panic!("expected release-binary runtime");
    };
    assert_eq!(asset, "thing-${target}.tar.gz");
    assert_eq!(bin.as_deref(), Some("thing"));

    parse(
        2,
        "capabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = [\"uv\", \"run\", \"worker\"]\nsystem = true\n",
    );
    let m = parse(
        5,
        r#"[[screenshots]]
path = "docs/screenshots/dashboard.png"
alt = "Dashboard card"
caption = "The plugin's dashboard card."

[[screenshots]]
path = "media/demo.gif"
alt = "Animated demo"
"#,
    );
    assert_eq!(m.screenshots[0].caption, "The plugin's dashboard card.");
    assert_eq!(m.screenshots[1].caption, "");
    let m = parse(7, "icon = \"git-branch\"\nicon_asset = \"assets/icon.png\"");
    assert_eq!(m.icon.as_deref(), Some("git-branch"));
    assert_eq!(m.icon_asset.as_deref(), Some("assets/icon.png"));
}

#[test]
fn invalid_manifests_report_each_problem() {
    let abs = if cfg!(windows) {
        "C:/tools/worker.exe"
    } else {
        "/usr/bin/worker"
    };
    let abs_runtime = format!("[runtime]\nkind = \"command\"\ncommand = [\"{abs}\"]\n");
    let too_many_screenshots = (0..9)
        .map(|i| format!("[[screenshots]]\npath = \"s{i}.png\"\nalt = \"s{i}\"\n"))
        .collect::<String>();
    let cases: Vec<(u32, &str, &[&str])> = vec![
        (
            2,
            "[[settings]]\nkey = \"mode\"\ntype = \"select\"\n\n[[settings]]\nkey = \"n\"\ntype = \"integer\"\nmin = 9\nmax = 1\n\n[setting_defaults]\n\"nosection\" = 1\n\n[[themes]]\nname = \"\"\npath = \"\"\n",
            &["select", "min must not exceed max", "setting_defaults key", "themes[0].name", "themes[0].path"],
        ),
        (
            2,
            "[[settings]]\nkey = \"retries\"\ntype = \"integer\"\nmin = 0\nmax = 5\ndefault = \"x\"\n\n[[settings]]\nkey = \"n\"\ntype = \"integer\"\nmax = 5\ndefault = 9\n\n[[settings]]\nkey = \"lo\"\ntype = \"integer\"\nmin = 10\ndefault = 1\n\n[[settings]]\nkey = \"mode\"\ntype = \"select\"\noptions = [\"fast\", \"slow\"]\ndefault = \"turbo\"\n",
            &[
                "settings[0].default does not match",
                "settings[1].default 9 is above max 5",
                "settings[2].default 1 is below min 10",
                "settings[3].default \"turbo\" is not one of the options",
            ],
        ),
        (4, "[[status]]\nid = \"\"\nlabel = \"x\"\n", &["status[0].id"]),
        (4, "aoe_version = \"not a range\"\n", &["aoe_version"]),
        (
            3,
            "aoe_version = \">=0.10\"\n\n[[status]]\nid = \"queue\"\n",
            &["status contributions require api_version >= 4", "aoe_version requires api_version >= 4"],
        ),
        (
            2,
            "[runtime]\nkind = \"command\"\ncommand = [\".venv/bin/worker\"]\n\n[[runtime.build]]\ncommand = [\"\"]\nplatforms = [\"linux\", \"plan9\"]\n",
            &["runtime.build[0].command", "plan9"],
        ),
        (
            2,
            "[runtime]\nkind = \"command\"\ncommand = [\"worker\"]\n",
            &["plugin-relative"],
        ),
        (2, &abs_runtime, &["plugin-relative"]),
        (
            2,
            "[runtime]\nkind = \"command\"\ncommand = [\".venv/bin/worker\"]\nsystem = true\n",
            &["system = true"],
        ),
        (2, "[runtime]\nkind = \"command\"\ncommand = []\n", &["runtime command must not be empty"]),
        (2, "[runtime]\nkind = \"command\"\ncommand = [\"\"]\n", &["empty arguments"]),
        (
            2,
            "[[commands]]\nid = \"\"\n\n[[keybinds]]\ncommand = \"\"\nkey = \"\"\n\n[[ui]]\nslot = \"status-bar\"\n",
            &["commands[0].id", "keybinds[0].command", "ui[0].id"],
        ),
        (
            7,
            "[[ui]]\nslot = \"composer-action\"\nid = \"voice\"\n",
            &["composer-action UI slots require api_version >= 8"],
        ),
        (
            24,
            "[[ui]]\nslot = \"composer-status\"\nid = \"ctx\"\n",
            &["composer-status UI slots require api_version >= 25"],
        ),
        (4, "[[screenshots]]\npath = \"a.png\"\nalt = \"a\"\n", &["screenshots require api_version >= 5"]),
        (5, "[[screenshots]]\npath = \"a.png\"\nalt = \"   \"\n", &["alt must not be empty"]),
        (5, &too_many_screenshots, &["at most 8 screenshots"]),
        (6, "icon = \"git-branch\"\n", &["icon requires api_version >= 7"]),
        (6, "icon_asset = \"assets/icon.png\"\n", &["icon_asset requires api_version >= 7"]),
    ];
    for (api, body, expected) in &cases {
        let messages = invalid(*api, body);
        for e in *expected {
            assert!(
                messages.iter().any(|m| m.contains(e)),
                "{e:?} not in {messages:?}"
            );
        }
    }
    match PluginManifest::from_toml_str(
        "id = \"a.b\"\nname = \"\"\nversion = \"\"\napi_version = 2\n",
    ) {
        Err(ManifestError::Invalid(m)) => {
            assert!(m.iter().any(|m| m.contains("name")), "{m:?}");
            assert!(m.iter().any(|m| m.contains("version must")), "{m:?}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    for bad in [
        "https://tracker.example.com/x.png",
        "/etc/passwd.png",
        "../secrets.png",
        "a/../../b.png",
        "C:/Windows/x.png",
        "no-extension",
        "evil.svg",
    ] {
        let m = invalid(
            5,
            &format!("[[screenshots]]\npath = \"{bad}\"\nalt = \"x\"\n"),
        );
        assert!(
            m.iter()
                .any(|e| e.contains("must be a repository-relative image path")),
            "{bad}"
        );
        let m = invalid(7, &format!("icon_asset = \"{bad}\"\n"));
        assert!(
            m.iter().any(|e| e.contains("icon_asset")
                && e.contains("must be a repository-relative image path")),
            "{bad}"
        );
    }
    for bad in ["GitHub", "git_branch", "-git", "git--branch", ""] {
        let m = invalid(7, &format!("icon = \"{bad}\"\n"));
        assert!(
            m.iter()
                .any(|e| e.contains("must be a lucide kebab-case icon name")),
            "{bad}"
        );
    }
}

#[test]
fn hard_parse_errors() {
    let unsupported = PluginManifest::from_toml_str(&manifest(9999, "")).unwrap_err();
    assert!(
        matches!(
            unsupported,
            ManifestError::UnsupportedApiVersion { found: 9999, .. }
        ),
        "{unsupported:?}"
    );
    // Removed slots (#3054) must fail rather than load inert.
    for body in [
        "frobnicate = true\n".to_string(),
        "[[panes]]\nid = \"x\"\ntitle = \"x\"\n".to_string(),
        "[[ui]]\nslot = \"sidebar\"\nid = \"panel\"\n".to_string(),
        "[[ui]]\nslot = \"settings-page\"\nid = \"panel\"\n".to_string(),
        "[[ui]]\nslot = \"tool-card-badge\"\nid = \"panel\"\n".to_string(),
    ] {
        let err = PluginManifest::from_toml_str(&manifest(13, &body)).unwrap_err();
        assert!(matches!(err, ManifestError::Parse(_)), "{body}: {err:?}");
    }
}

#[test]
fn path_icon_and_hash_helpers() {
    for (toml_slot, slot) in [
        ("status-bar", UiSlot::StatusBar),
        ("row-badge", UiSlot::RowBadge),
        ("pane", UiSlot::Pane),
        ("composer-action", UiSlot::ComposerAction),
        ("composer-status", UiSlot::ComposerStatus),
        ("home-pane", UiSlot::HomePane),
        ("notification", UiSlot::Notification),
    ] {
        assert_eq!(slot.as_str(), toml_slot);
    }
    assert!(screenshot_path_ok("docs/shot.png"));
    assert!(screenshot_path_ok("a/b/c.gif"));
    for bad in [
        "docs\\shot.png",
        "..\\x.png",
        "C:\\x.png",
        "/abs.png",
        "https://x/y.png",
        "a/../b.png",
        "noext",
        "evil.svg",
        "",
    ] {
        assert!(!screenshot_path_ok(bad), "{bad:?}");
    }
    assert!(lucide_icon_name_ok("git-branch"));
    assert!(lucide_icon_name_ok("puzzle"));
    for bad in ["GitHub", "git_branch", "-git", "git-", "git--branch", ""] {
        assert!(!lucide_icon_name_ok(bad), "{bad:?}");
    }
    let bytes = b"id = \"acme.thing\"\n";
    let a = PluginManifest::hash_bytes(bytes);
    assert_eq!(a, PluginManifest::hash_bytes(bytes));
    assert!(a.starts_with("sha256:"));
    assert_ne!(a, PluginManifest::hash_bytes(b"different"));
}

#[test]
fn suggestions_require_current_schema_and_worker() {
    let worker = "capabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = [\"./worker\"]\n";
    let body = format!(
        "{worker}[suggestions]\n[[suggestions.sources]]\ncriterion = \"review_requested\"\nlabel = \"Review requests\"\nvariables = [\"pr_number\", \"repo\"]\n[[suggestions.sources]]\ncriterion = \"ci_failed\"\nlabel = \"Failing checks\"\ndefault_enabled = false\n"
    );
    let spec = parse(23, &body).suggestions.expect("declared");
    assert_eq!(spec.sources.len(), 2);
    assert_eq!(spec.sources[0].variables, vec!["pr_number", "repo"]);
    assert!(spec.sources[0].default_enabled);
    assert!(!spec.sources[1].default_enabled);
    assert!(parse(23, "").suggestions.is_none());

    let source = "[[suggestions.sources]]\ncriterion = \"a\"\nlabel = \"A\"\n";
    let long = "x".repeat(257);
    let cases = [
        (22, body.clone(), "api_version >= 23"),
        (
            23,
            format!("{worker}[suggestions]\n"),
            "at least one source",
        ),
        (23, source.to_string(), "runtime worker"),
        (
            23,
            format!("[runtime]\nkind = \"command\"\ncommand = [\"./w\"]\n{source}"),
            "runtime.worker capability",
        ),
        (23, format!("{worker}{source}{source}"), "duplicate"),
        (
            23,
            format!("{worker}[[suggestions.sources]]\ncriterion = \"{long}\"\nlabel = \"A\"\n"),
            "at most 256 bytes",
        ),
        (
            23,
            format!("{worker}[[suggestions.sources]]\ncriterion = \"a\"\nlabel = \" \"\n"),
            "needs a label",
        ),
        (
            23,
            format!("{worker}{source}variables = [\"Pr-Number\"]\n"),
            "[a-z0-9_]+",
        ),
        (
            23,
            format!("{worker}{source}variables = [\"repo\", \"repo\"]\n"),
            "duplicate variables",
        ),
    ];
    for (version, body, expected) in cases {
        assert!(
            invalid(version, &body).iter().any(|e| e.contains(expected)),
            "expected {expected:?} for {body:?}"
        );
    }
}

#[test]
fn suggestion_items_reject_host_owned_fields() {
    let item = |extra: serde_json::Value| {
        let mut v = serde_json::json!({
            "id":"w", "title":"W", "reason":"r", "source_url":"https://example.com/1",
            "source_ref":"x", "preview":"p", "criterion":"c",
            "draft":{"title":"W","prompt":"Do"}
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<aoe_plugin_api::Suggestion>(v)
    };
    assert!(item(serde_json::json!({})).is_ok());
    assert!(item(serde_json::json!({"variables":{"repo":"a/b"}})).is_ok());
    assert!(item(serde_json::json!({"provider_id":"forged"})).is_err());
    assert!(!item(serde_json::json!({})).unwrap().stale);
    assert!(item(serde_json::json!({"stale":true})).unwrap().stale);
}

#[test]
fn composer_status_slot_parses_at_api_25() {
    let parsed = parse(25, "[[ui]]\nslot = \"composer-status\"\nid = \"ctx\"\n");
    assert_eq!(parsed.ui[0].slot, UiSlot::ComposerStatus);
    assert!(parsed.ui[0].slot.is_per_session());
}

const HOOKS: &str = "[agent_integrations.hooks]\n\
    settings_rel_path = \".example/settings.json\"\n\
    config_dir_env_var = \"EXAMPLE_CONFIG_DIR\"\n\
    format = \"json_settings\"\n\
    [[agent_integrations.hooks.events]]\n\
    event = \"PreToolUse\"\n\
    status = \"running\"\n\
    waiting_tools = [\"AskUserQuestion\"]\n\
    [[agent_integrations.hooks.events]]\n\
    event = \"Notification\"\n\
    matcher = \"permission_prompt\"\n\
    status = \"waiting\"\n";
const TRUST: &str = "[agent_integrations.trust]\n\
    file = \".example.json\"\n\
    location = \"config_dir_or_home\"\n\
    config_dir_env_var = \"EXAMPLE_CONFIG_DIR\"\n\
    pointer = \"/projects/{path}/trusted\"\n\
    value = true\n\
    sandbox = \"always\"\n";

fn agent(tables: &str) -> String {
    format!(
        "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n\
         [agent_integrations.launch.container_env]\nEXAMPLE_CONFIG_DIR = \"/opt/example\"\n{tables}"
    )
}

#[test]
fn hooks_and_trust_require_api_26() {
    let body = agent(&format!("{HOOKS}{TRUST}"));
    let parsed = parse(28, &body);
    let integration = &parsed.agent_integrations[0];
    assert_eq!(integration.hooks.as_ref().unwrap().events.len(), 2);
    assert!(integration.trust.is_some());
    assert_eq!(
        PluginManifest::from_toml_str(&toml::to_string(&parsed).unwrap())
            .expect("round-trips")
            .agent_integrations[0],
        *integration
    );

    let errors = invalid(27, &body);
    for table in ["hooks", "trust"] {
        let want = format!("agent_integrations[0].{table} requires api_version >= 28");
        assert!(
            errors.iter().any(|error| error.contains(&want)),
            "{errors:?}"
        );
    }
}

#[test]
fn agent_display_name_and_icon_are_validated() {
    let body = |fields: &str| {
        format!("[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n{fields}")
    };
    let ok = parse(
        29,
        &body("display_name = \"Example CLI\"\nicon = \"bot\"\n"),
    );
    let integration = &ok.agent_integrations[0];
    assert_eq!(integration.display_name.as_deref(), Some("Example CLI"));
    assert_eq!(integration.icon.as_deref(), Some("bot"));

    let gated = invalid(28, &body("display_name = \"Example\"\nicon = \"bot\"\n"));
    for field in ["display_name", "icon"] {
        let want = format!("agent_integrations[0].{field} requires api_version >= 29");
        assert!(gated.iter().any(|error| error.contains(&want)), "{gated:?}");
    }

    let long = "x".repeat(41);
    for (fields, want) in [
        ("display_name = \"\"\n".to_string(), "display_name"),
        (format!("display_name = \"{long}\"\n"), "display_name"),
        ("display_name = \" Padded\"\n".to_string(), "display_name"),
        (
            "display_name = \"A\\u001b[2J\"\n".to_string(),
            "display_name",
        ),
        ("display_name = \"A\\u202eB\"\n".to_string(), "display_name"),
        ("icon = \"Not An Icon\"\n".to_string(), "icon"),
    ] {
        let errors = invalid(29, &body(&fields));
        assert!(
            errors.iter().any(|error| error.contains(want)),
            "{fields}: {errors:?}"
        );
    }
}

#[test]
fn hooks_declarations_are_validated() {
    let with = |from: &str, to: &str| agent(&HOOKS.replace(from, to));
    let cases = [
        (with(".example/settings.json", "/etc/settings.json"), "settings_rel_path"),
        (with(".example/settings.json", "../settings.json"), "settings_rel_path"),
        (with(".example/settings.json", ".example/./settings.json"), "settings_rel_path"),
        (with(".example/settings.json", ""), "settings_rel_path"),
        (with(".example/settings.json", ".zshrc"), "settings_rel_path"),
        (with(".example/settings.json", ".example:invalid/settings.json"), "settings_rel_path"),
        (with(".example/settings.json", ".ex,ample/settings.json"), "settings_rel_path"),
        (with(".example/settings.json", ".ex\\\\ample/settings.json"), "settings_rel_path"),
        (with(".example/settings.json", ".example/.zshrc"), "settings_rel_path"),
        (with(".example/settings.json", "settings.json"), "settings_rel_path"),
        (with("EXAMPLE_CONFIG_DIR", "1BAD"), "config_dir_env_var"),
        (with("\"PreToolUse\"", "\"Bogus\""), "events[0].event"),
        (with("json_settings", "toml"), "unknown variant"),
        (with("status = \"running\"", "status = \"busy\""), "unknown variant"),
        (
            agent(&format!(
                "{HOOKS}[[agent_integrations.hooks.events]]\nevent = \"Stop\"\n"
            )),
            "needs a status or an identity_field",
        ),
        (
            agent(&format!(
                "{HOOKS}[[agent_integrations.hooks.events]]\nevent = \"Notification\"\nmatcher = \"permission_prompt\"\n"
            )),
            "repeats an event and matcher",
        ),
        (
            agent(
                "[agent_integrations.hooks]\nsettings_rel_path = \".x/s.json\"\nformat = \"json_settings\"\nevents = []\n",
            ),
            "hooks.events must hold",
        ),
    ];
    for (body, want) in cases {
        match PluginManifest::from_toml_str(&manifest(28, &body)) {
            Err(error) => assert!(error.to_string().contains(want), "{want}: {error}"),
            Ok(_) => panic!("{want}: should be rejected\n{body}"),
        }
    }
    // The same event may repeat with different matchers.
    parse(28, &agent(&format!("{HOOKS}[[agent_integrations.hooks.events]]\nevent = \"Notification\"\nmatcher = \"idle_prompt\"\nstatus = \"idle\"\n")));
}

#[test]
fn trust_declarations_are_validated() {
    let with = |from: &str, to: &str| agent(&TRUST.replace(from, to));
    let pointers = [
        "projects/{path}/trusted",
        "/projects/trusted",
        "/projects/{path}/{path}",
        "/projects//{path}/trusted",
        "/projects/{path}/0",
        "/projects/x{path}/trusted",
        "/projects/{path}/a~1b",
        "/a/b/c/d/e/f/g/{path}/h",
    ];
    for pointer in pointers {
        let body = with("/projects/{path}/trusted", pointer);
        assert!(
            PluginManifest::from_toml_str(&manifest(28, &body))
                .unwrap_err()
                .to_string()
                .contains("trust.pointer"),
            "{pointer}"
        );
    }
    for file in ["sub/.example.json", "../x.json", "/abs.json", ".."] {
        let body = with(".example.json", file);
        assert!(
            PluginManifest::from_toml_str(&manifest(28, &body))
                .unwrap_err()
                .to_string()
                .contains("trust.file"),
            "{file}"
        );
    }
    for (from, to, want) in [
        (
            "location = \"config_dir_or_home\"",
            "location = \"elsewhere\"",
            "unknown variant",
        ),
        (
            "sandbox = \"always\"",
            "sandbox = \"sometimes\"",
            "unknown variant",
        ),
        ("value = true", "value = 3", "data did not match"),
    ] {
        let error = PluginManifest::from_toml_str(&manifest(28, &with(from, to))).unwrap_err();
        assert!(error.to_string().contains(want), "{want}: {error}");
    }
    parse(28, &with("value = true", "value = \"TRUST_FOLDER\""));
    parse(28, &with("/projects/{path}/trusted", "/{path}"));
    parse(28, &with("sandbox = \"always\"", "sandbox = \"yolo_only\""));
}

#[test]
fn declaration_bounds_each_reject() {
    let hooks_with = |from: &str, to: &str| agent(&HOOKS.replace(from, to));
    let trust_with = |from: &str, to: &str| agent(&TRUST.replace(from, to));
    let with_env = |tables: &str, var: &str, dir: &str| {
        format!(
            "[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n\
             [agent_integrations.launch.container_env]\n{var} = \"{dir}\"\n{}",
            tables.replace("EXAMPLE_CONFIG_DIR", var)
        )
    };
    let event = |n: usize| {
        format!("[[agent_integrations.hooks.events]]\nevent = \"Stop\"\nstatus = \"idle\"\nmatcher = \"m{n}\"\n")
    };
    let long_path = format!("{}/settings.json", "d".repeat(260));
    let too_many_tools = format!(
        "waiting_tools = [{}]",
        (0..33)
            .map(|n| format!("\"t{n}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    // Without a config-dir variable the hooks directory is `/root/.example.json`.
    let home_relative = |hooks_path: &str| {
        let tables = format!("{HOOKS}{TRUST}")
            .replace("config_dir_env_var = \"EXAMPLE_CONFIG_DIR\"\n", "")
            .replace(".example/settings.json", hooks_path);
        agent(&tables)
    };
    parse(28, &home_relative(".example/settings.json"));
    let cases: Vec<(String, &str)> = vec![
        (
            home_relative(".example.json/settings.json"),
            "must not be, or contain, the sandbox path",
        ),
        (
            agent(&format!(
                "{HOOKS}{}",
                TRUST.replace(".example.json", "settings.json")
            )),
            "must not be, or contain, the sandbox path",
        ),
        (
            hooks_with(".example/settings.json", &long_path),
            "settings_rel_path",
        ),
        (
            agent(&format!(
                "{}{}",
                HOOKS
                    .split("[[agent_integrations.hooks.events]]")
                    .next()
                    .unwrap(),
                (0..33).map(event).collect::<String>()
            )),
            "hooks.events must hold",
        ),
        (
            hooks_with("waiting_tools = [\"AskUserQuestion\"]", &too_many_tools),
            "waiting_tools",
        ),
        (
            hooks_with(
                "waiting_tools = [\"AskUserQuestion\"]",
                &format!("waiting_tools = [\"{}\"]", "w".repeat(129)),
            ),
            "waiting_tools",
        ),
        (
            hooks_with("permission_prompt", &"m".repeat(257)),
            "matcher must be 1..=256",
        ),
        (
            trust_with("value = true", &format!("value = \"{}\"", "v".repeat(257))),
            "trust.value must be 1..=256",
        ),
        (trust_with(".example.json", ".example"), "trust.file"),
        (trust_with(".example.json", "a:b.json"), "trust.file"),
        (trust_with(".example.json", "a,b.json"), "trust.file"),
        (trust_with(".example.json", ".example.yaml"), "trust.file"),
        (
            trust_with(
                "/projects/{path}/trusted",
                &format!("/{}/{{path}}", ["s"; 8].join("/")),
            ),
            "trust.pointer",
        ),
        (
            trust_with(
                "/projects/{path}/trusted",
                &format!("/{}/{{path}}", "s".repeat(257)),
            ),
            "trust.pointer",
        ),
    ];
    for (body, want) in cases {
        match PluginManifest::from_toml_str(&manifest(28, &body)) {
            Err(error) => assert!(error.to_string().contains(want), "{want}: {error}"),
            Ok(_) => panic!("{want}: should be rejected\n{body}"),
        }
    }

    // Another file in the shared directory is fine.
    parse(
        28,
        &agent(&format!(
            "{HOOKS}{}",
            TRUST.replace(".example.json", "other.json")
        )),
    );

    // Config-directory variables: credential-shaped, reserved and undeclared names.
    for var in [
        "HOME",
        "PATH",
        "AOE_CONFIG_DIR",
        "LD_LIBRARY_DIR",
        "DYLD_FALLBACK_DIR",
        "XDG_CONFIG_HOME",
        "GITHUB_TOKEN",
        "API_KEY_DIR",
        "DB_PASSWORD",
        "CLIENT_SECRET_DIR",
        "1BAD",
        "BAD-NAME",
    ] {
        for tables in [HOOKS, TRUST] {
            let body = with_env(tables, var, "/opt/example");
            let error = PluginManifest::from_toml_str(&manifest(28, &body))
                .map(|_| ())
                .expect_err(var);
            assert!(
                error.to_string().contains("config_dir_env_var"),
                "{var}: {error}"
            );
        }
    }
    let undeclared =
        format!("[[agent_integrations]]\ntool = \"example\"\ncommand = \"example\"\n{HOOKS}");
    assert!(PluginManifest::from_toml_str(&manifest(28, &undeclared))
        .unwrap_err()
        .to_string()
        .contains("launch.container_env key"));

    // Container directories the sandbox would mount read-write.
    let too_long = format!("/{}", "d".repeat(300));
    for dir in [
        "relative/dir",
        "/",
        "/root",
        "/opt/../etc",
        "/opt/./x",
        "/opt//x",
        "/opt/x/",
        "/opt/a:b",
        "/opt/a,b",
        "/workspace",
        "/workspace/project",
        "/aoe/artifacts",
        "/tmp/aoe-hooks/x",
        "/proc/self",
        &too_long,
    ] {
        let body = with_env(HOOKS, "EXAMPLE_CONFIG_DIR", dir);
        let error = PluginManifest::from_toml_str(&manifest(28, &body))
            .map(|_| ())
            .expect_err(dir);
        assert!(
            error.to_string().contains("container_env"),
            "{dir}: {error}"
        );
    }
    for dir in ["/opt/example", "/root/.example", "/home/agent/.cfg"] {
        parse(28, &with_env(HOOKS, "EXAMPLE_CONFIG_DIR", dir));
    }
}

#[test]
fn identity_hooks_need_a_bare_executable_name() {
    let body = |command: &str, identity: bool| {
        let events = if identity {
            "[[agent_integrations.hooks.events]]\nevent = \"SessionStart\"\nidentity_field = \"session_id\"\n"
        } else {
            ""
        };
        format!(
            "[[agent_integrations]]\ntool = \"example\"\ncommand = \"{command}\"\n\
             [agent_integrations.launch.container_env]\nEXAMPLE_CONFIG_DIR = \"/opt/example\"\n\
             {HOOKS}{events}"
        )
    };
    for command in [
        "example+cli",
        "/opt/bin/example",
        "FOO=bar ex@mple",
        "ex,ample",
    ] {
        let error = PluginManifest::from_toml_str(&manifest(28, &body(command, true)))
            .map(|_| ())
            .expect_err(command);
        assert!(
            error.to_string().contains("bare name"),
            "{command}: {error}"
        );
        // Without an identity event there is no publisher guard to lose.
        parse(28, &body(command, false));
    }
    for command in [
        "example-cli",
        "example.v2 --flag",
        "PATH_HINT=x example_cli",
    ] {
        parse(28, &body(command, true));
    }
}

const ACP_CAP: &str = "capabilities = [\"acp.agent.provide\"]\n";
const ACP: &str = "[agent_integrations.acp]\n\
    id = \"example\"\n\
    aliases = [\"example-cli\"]\n\
    display_name = \"Example\"\n\
    failure_policy = \"neutral_boot\"\n\
    [agent_integrations.acp.proxy]\n\
    command = \"bin/proxy\"\n\
    protocol_version = 1\n\
    primitive_schema_version = 1\n\
    [agent_integrations.acp.adapter]\n\
    upstream_package = \"@example/acp\"\n\
    upstream_pin = \"1.2.3\"\n\
    package_dir = \"adapter\"\n\
    [agent_integrations.acp.adapter.binary]\n\
    digests = \"sha256_asset\"\n\
    [agent_integrations.acp.adapter.binary.assets]\n\
    aarch64-apple-darwin = \"example-darwin-arm64.zip\"\n\
    [agent_integrations.acp.env]\n\
    allowlist = [\"EXAMPLE_API_KEY\", \"EXAMPLE_HOME\"]\n\
    host_only = [\"EXAMPLE_HOME\"]\n\
    [agent_integrations.acp.env.defaults]\n\
    EXAMPLE_MODE = \"fast\"\n\
    [agent_integrations.acp.compat]\n\
    expected_agent_name = \"example\"\n\
    min_version = \"1.0.0\"\n\
    [agent_integrations.acp.compat.capability_min_versions]\n\
    fork = \"1.1.0\"\n\
    [[agent_integrations.acp.profile.modes]]\n\
    id = \"plan\"\n\
    approval_class = \"guarded\"\n";

fn acp_body(acp: &str) -> String {
    format!("{ACP_CAP}{}", agent(acp))
}

#[test]
fn acp_declaration_is_gated_and_round_trips() {
    let body = acp_body(ACP);
    let parsed = parse(30, &body);
    let acp = parsed.agent_integrations[0].acp.as_ref().unwrap();
    assert_eq!(acp.id, "example");
    assert_eq!(acp.profile.modes.len(), 1);
    assert_eq!(
        PluginManifest::from_toml_str(&toml::to_string(&parsed).unwrap())
            .expect("round-trips")
            .agent_integrations[0],
        parsed.agent_integrations[0]
    );

    let errors = invalid(29, &body);
    let want = "agent_integrations[0].acp requires api_version >= 30";
    assert!(errors.iter().any(|e| e.contains(want)), "{errors:?}");

    let errors = invalid(30, &agent(ACP));
    let want = "acp requires the acp.agent.provide capability";
    assert!(errors.iter().any(|e| e.contains(want)), "{errors:?}");

    let unknown = ACP.replace("display_name", "unknown_field = 1\ndisplay_name");
    let err = PluginManifest::from_toml_str(&manifest(30, &acp_body(&unknown))).unwrap_err();
    assert!(err.to_string().contains("unknown_field"), "{err}");
}

#[test]
fn acp_declaration_rejects_bad_fields() {
    let cases: [(&str, &str, &str); 25] = [
        ("id = \"example\"", "id = \"Bad Id\"", "acp.id"),
        ("id = \"example\"", "id = \"\"", "acp.id"),
        ("[\"example-cli\"]", "[\"example\"]", "aliases"),
        ("[\"example-cli\"]", "[\"a\", \"a\"]", "aliases"),
        ("display_name = \"Example\"", "display_name = \"\"", "display_name"),
        ("command = \"bin/proxy\"", "command = \"/bin/proxy\"", "proxy.command"),
        ("command = \"bin/proxy\"", "command = \"../proxy\"", "proxy.command"),
        ("command = \"bin/proxy\"", "command = \"\"", "proxy.command"),
        ("protocol_version = 1", "protocol_version = 2", "protocol_version"),
        ("primitive_schema_version = 1", "primitive_schema_version = 0", "primitive_schema_version"),
        ("upstream_pin = \"1.2.3\"", "upstream_pin = \"^1.2.3\"", "upstream_pin"),
        ("upstream_pin = \"1.2.3\"", "upstream_pin = \">=1.2\"", "upstream_pin"),
        ("package_dir = \"adapter\"", "package_dir = \"../adapter\"", "package_dir"),
        ("package_dir = \"adapter\"", "package_dir = \"/adapter\"", "package_dir"),
        ("\"example-darwin-arm64.zip\"", "\"\"", "binary.assets"),
        ("[agent_integrations.acp.adapter.binary.assets]", "[agent_integrations.acp.adapter.binary.assets]\nlinux = \"a/b\"", "binary.assets"),
        ("allowlist = [\"EXAMPLE_API_KEY\"", "allowlist = [\"EXAMPLE_*\"", "env.allowlist"),
        ("allowlist = [\"EXAMPLE_API_KEY\"", "allowlist = [\"1BAD\"", "env.allowlist"),
        ("host_only = [\"EXAMPLE_HOME\"]", "host_only = [\"OTHER\"]", "host_only"),
        ("EXAMPLE_MODE = \"fast\"", "EXAMPLE_MODE = \"a\\u0000b\"", "env.defaults"),
        ("EXAMPLE_MODE = \"fast\"", "\"BAD-NAME\" = \"x\"", "env.defaults"),
        ("min_version = \"1.0.0\"", "min_version = \"one\"", "compat"),
        ("fork = \"1.1.0\"", "fork = \"latest\"", "compat"),
        ("id = \"plan\"", "id = \"\"", "profile.modes"),
        ("approval_class = \"guarded\"\n", "approval_class = \"guarded\"\n[[agent_integrations.acp.profile.modes]]\nid = \"plan\"\napproval_class = \"interactive\"\n", "profile.modes"),
    ];
    for (from, to, want) in cases {
        assert!(ACP.contains(from), "{from}");
        let errors = invalid(30, &acp_body(&ACP.replace(from, to)));
        assert!(
            errors.iter().any(|e| e.contains("acp") && e.contains(want)),
            "{to}: {errors:?}"
        );
    }
}
