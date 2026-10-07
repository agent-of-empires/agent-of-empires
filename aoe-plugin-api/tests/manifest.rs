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
    let err = m.host_compat("0.13.0").unwrap_err();
    assert!(
        err.contains("plugin requires aoe") && err.contains("0.13.0"),
        "{err}"
    );
    assert!(parse(4, "").host_compat("99.0.0").is_ok());
}

#[test]
fn runtime_specs_parse() {
    let m = parse(
        2,
        r#"[runtime]
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
        "[runtime]\nkind = \"release-binary\"\nasset = \"thing-${target}.tar.gz\"\nbin = \"thing\"\n",
    );
    let Some(RuntimeSpec::ReleaseBinary { asset, bin }) = m.runtime else {
        panic!("expected release-binary runtime");
    };
    assert_eq!(asset, "thing-${target}.tar.gz");
    assert_eq!(bin.as_deref(), Some("thing"));

    parse(
        2,
        "[runtime]\nkind = \"command\"\ncommand = [\"uv\", \"run\", \"worker\"]\nsystem = true\n",
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
