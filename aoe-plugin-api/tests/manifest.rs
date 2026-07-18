use aoe_plugin_api::{ManifestError, PluginManifest, RuntimeSpec, SettingType, UiSlot};

#[test]
fn minimal_manifest_parses_and_round_trips() {
    let toml = r#"
id = "aoe.web"
name = "Web Dashboard"
version = "1.0.0"
api_version = 2
description = "The aoe serve web dashboard."
"#;
    let manifest = PluginManifest::from_toml_str(toml).expect("valid manifest parses");
    assert_eq!(manifest.id.as_str(), "aoe.web");
    assert_eq!(manifest.name, "Web Dashboard");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.api_version, 2);

    let serialized = toml::to_string(&manifest).expect("serializes");
    let reparsed = PluginManifest::from_toml_str(&serialized).expect("round-trips");
    assert_eq!(reparsed.id.as_str(), "aoe.web");
}

#[test]
fn api_version_1_still_parses() {
    // The bundled aoe.web manifest still targets api_version 1; an older
    // manifest must keep loading on a newer host.
    let toml = r#"
id = "aoe.web"
name = "Web Dashboard"
version = "1.0.0"
api_version = 1
"#;
    PluginManifest::from_toml_str(toml).expect("api_version 1 stays supported");
}

#[test]
fn description_defaults_to_empty() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2
"#;
    let manifest = PluginManifest::from_toml_str(toml).expect("description is optional");
    assert!(manifest.description.is_empty());
}

#[test]
fn contribution_sections_parse() {
    let toml = r#"
id = "acme.kit"
name = "Kit"
version = "0.1.0"
api_version = 2
capabilities = ["session.read", "net"]

[[commands]]
id = "do-thing"
title = "Do Thing"

[[keybinds]]
command = "plugin.acme.kit.do-thing"
key = "Ctrl+K"

[[settings]]
key = "endpoint"
label = "Endpoint"

[[ui]]
slot = "status-bar"
id = "panel"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("contribution sections parse");
    assert_eq!(m.capabilities.len(), 2);
    assert!(m.capabilities[0].is_known());
    assert_eq!(m.commands.len(), 1);
    assert_eq!(m.keybinds[0].key, "Ctrl+K");
    assert_eq!(m.settings[0].key, "endpoint");
    assert_eq!(m.settings[0].value_type, SettingType::String);
    assert_eq!(m.ui[0].slot, UiSlot::StatusBar);
}

#[test]
fn typed_settings_and_defaults_and_themes_parse() {
    let toml = r#"
id = "acme.kit"
name = "Kit"
version = "0.1.0"
api_version = 2

[[settings]]
key = "enabled"
label = "Enabled"
type = "bool"
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
"#;
    let m = PluginManifest::from_toml_str(toml).expect("typed contributions parse");
    assert_eq!(m.settings[0].value_type, SettingType::Bool);
    assert_eq!(m.settings[1].value_type, SettingType::Integer);
    assert_eq!(m.settings[1].min, Some(0));
    assert!(m.settings[1].advanced);
    assert_eq!(m.settings[2].options, ["fast", "slow"]);
    assert_eq!(
        m.setting_defaults.get("theme.idle_decay_minutes"),
        Some(&toml::Value::Integer(10))
    );
    assert_eq!(m.themes[0].name, "kit-dark");
    assert_eq!(m.themes[0].path, "themes/dark.toml");
}

#[test]
fn invalid_typed_settings_and_themes_collect_problems() {
    let toml = r#"
id = "acme.kit"
name = "Kit"
version = "0.1.0"
api_version = 2

[[settings]]
key = "mode"
type = "select"

[[settings]]
key = "n"
type = "integer"
min = 9
max = 1

[setting_defaults]
"nosection" = 1

[[themes]]
name = ""
path = ""
"#;
    let messages = match PluginManifest::from_toml_str(toml).unwrap_err() {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages.iter().any(|m| m.contains("select")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("min must not exceed max")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("setting_defaults key")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("themes[0].name")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("themes[0].path")),
        "{messages:?}"
    );
}

#[test]
fn setting_default_must_match_type() {
    let toml = r#"
id = "acme.kit"
name = "Kit"
version = "0.1.0"
api_version = 2

[[settings]]
key = "retries"
type = "integer"
min = 0
max = 5
default = "not-an-int"

[[settings]]
key = "n"
type = "integer"
max = 5
default = 9

[[settings]]
key = "lo"
type = "integer"
min = 10
default = 1

[[settings]]
key = "mode"
type = "select"
options = ["fast", "slow"]
default = "turbo"
"#;
    let messages = match PluginManifest::from_toml_str(toml).unwrap_err() {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages
            .iter()
            .any(|m| m.contains("settings[0].default does not match")),
        "{messages:?}"
    );
    // Single-sided bounds are each enforced.
    assert!(
        messages
            .iter()
            .any(|m| m.contains("settings[1].default 9 is above max 5")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("settings[2].default 1 is below min 10")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("settings[3].default") && m.contains("not one of the options")),
        "{messages:?}"
    );
}

#[test]
fn panes_section_is_rejected() {
    // panes is not a manifest section: it ships as a `ui` slot kind (#2432).
    // deny_unknown_fields makes a `[[panes]]` block a hard parse error. status
    // is now a real section (#2386), asserted separately below.
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4

[[panes]]
id = "x"
title = "x"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(err, ManifestError::Parse(_)),
        "[[panes]] should be a hard parse error, got {err:?}"
    );
}

#[test]
fn status_section_parses() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4

[[status]]
id = "queue"
label = "Queue depth"

[[status]]
id = "nolabel"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("status section parses");
    assert_eq!(m.status[0].id, "queue");
    assert_eq!(m.status[0].label, "Queue depth");
    assert_eq!(m.status[1].id, "nolabel");
    assert_eq!(m.status[1].label, "", "label defaults to empty");
}

#[test]
fn invalid_status_collects_problem() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4

[[status]]
id = ""
label = "x"
"#;
    let messages = match PluginManifest::from_toml_str(toml).unwrap_err() {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages.iter().any(|m| m.contains("status[0].id")),
        "{messages:?}"
    );
}

#[test]
fn aoe_version_parses_and_validates() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4
aoe_version = ">=0.10, <0.12"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("valid aoe_version parses");
    assert_eq!(m.aoe_version.as_deref(), Some(">=0.10, <0.12"));

    let bad = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4
aoe_version = "not a range"
"#;
    let messages = match PluginManifest::from_toml_str(bad).unwrap_err() {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages.iter().any(|m| m.contains("aoe_version")),
        "{messages:?}"
    );
}

#[test]
fn host_compat_in_and_out_of_range() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4
aoe_version = ">=0.10, <0.12"
"#;
    let m = PluginManifest::from_toml_str(toml).unwrap();
    assert!(m.host_compat("0.11.0").is_ok());
    let err = m.host_compat("0.13.0").unwrap_err();
    assert!(err.contains("plugin requires aoe"), "{err}");
    assert!(err.contains("0.13.0"), "{err}");

    // A manifest with no declared range is always compatible.
    let unbounded = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 4
"#;
    let m = PluginManifest::from_toml_str(unbounded).unwrap();
    assert!(m.host_compat("99.0.0").is_ok());
}

#[test]
fn api_version_4_fields_require_bump() {
    // Using a v4 field at an older api_version is rejected, so an author bumps
    // to 4 and pre-4 hosts route to "upgrade aoe" instead of "unknown field".
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 3
aoe_version = ">=0.10"

[[status]]
id = "queue"
"#;
    let messages = match PluginManifest::from_toml_str(toml).unwrap_err() {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages
            .iter()
            .any(|m| m.contains("status contributions require api_version >= 4")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("aoe_version requires api_version >= 4")),
        "{messages:?}"
    );
}

#[test]
fn unknown_capability_string_parses_but_reports_unknown() {
    // Capabilities are open strings: a capability this host does not recognize
    // still parses (forward compatibility); the host rejects it at install.
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2
capabilities = ["some.future.cap"]
"#;
    let m = PluginManifest::from_toml_str(toml).expect("unknown capability still parses");
    assert!(!m.capabilities[0].is_known());
}

#[test]
fn runtime_command_parses() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = ["python3", "worker.py"]
system = true
"#;
    let m = PluginManifest::from_toml_str(toml).expect("runtime command parses");
    match m.runtime.expect("has runtime") {
        RuntimeSpec::Command {
            command,
            system,
            build,
        } => {
            assert_eq!(command, ["python3", "worker.py"]);
            assert!(system);
            assert!(build.is_empty());
        }
        other => panic!("expected command runtime, got {other:?}"),
    }
}

#[test]
fn runtime_command_build_steps_parse() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = [".venv/bin/worker"]

[[runtime.build]]
command = ["python3", "-m", "venv", ".venv"]

[[runtime.build]]
command = [".venv/bin/pip", "install", "."]
platforms = ["linux", "macos"]
"#;
    let m = PluginManifest::from_toml_str(toml).expect("build steps parse");
    match m.runtime.expect("has runtime") {
        RuntimeSpec::Command {
            command,
            system,
            build,
        } => {
            assert_eq!(command, [".venv/bin/worker"]);
            assert!(!system);
            assert_eq!(build.len(), 2);
            assert_eq!(build[0].command, ["python3", "-m", "venv", ".venv"]);
            assert!(build[0].platforms.is_empty());
            assert_eq!(build[1].command, [".venv/bin/pip", "install", "."]);
            assert_eq!(build[1].platforms, ["linux", "macos"]);
        }
        other => panic!("expected command runtime, got {other:?}"),
    }
}

#[test]
fn build_step_empty_command_and_unknown_platform_rejected() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = [".venv/bin/worker"]

[[runtime.build]]
command = [""]
platforms = ["linux", "plan9"]
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    let messages = match err {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages
            .iter()
            .any(|m| m.contains("runtime.build[0].command")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("runtime.build[0].platforms") && m.contains("plan9")),
        "{messages:?}"
    );
}

/// The worker entrypoint must be plugin-relative by default; a bare program
/// name is rejected unless the manifest opts into a PATH dependency with
/// `system = true`.
#[test]
fn bare_worker_program_requires_system_opt_in() {
    let manifest = |line: &str| {
        format!(
            r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
{line}
"#
        )
    };

    // Bare name, no opt-in: rejected, and the message points at the fix.
    let err = PluginManifest::from_toml_str(&manifest("command = [\"worker\"]")).unwrap_err();
    match err {
        ManifestError::Invalid(messages) => assert!(
            messages
                .iter()
                .any(|m| m.contains("plugin-relative") && m.contains("system = true")),
            "{messages:?}"
        ),
        other => panic!("expected Invalid, got {other:?}"),
    }

    // Same bare name with the opt-in parses.
    PluginManifest::from_toml_str(&manifest(
        "command = [\"uv\", \"run\", \"worker\"]\nsystem = true",
    ))
    .expect("system opt-in accepts a bare program name");

    // A plugin-relative path is the default-accepted shape.
    PluginManifest::from_toml_str(&manifest("command = [\".venv/bin/worker\"]"))
        .expect("plugin-relative entrypoint accepted without opt-in");

    // An absolute path pins a host path and is rejected in both modes.
    let abs = if cfg!(windows) {
        "C:/tools/worker.exe"
    } else {
        "/usr/bin/worker"
    };
    assert!(matches!(
        PluginManifest::from_toml_str(&manifest(&format!("command = [\"{abs}\"]"))).unwrap_err(),
        ManifestError::Invalid(_)
    ));

    // `system = true` with a path is contradictory and rejected.
    let err =
        PluginManifest::from_toml_str(&manifest("command = [\".venv/bin/worker\"]\nsystem = true"))
            .unwrap_err();
    match err {
        ManifestError::Invalid(messages) => assert!(
            messages.iter().any(|m| m.contains("system = true")),
            "{messages:?}"
        ),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn runtime_release_binary_parses() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "release-binary"
asset = "thing-${target}.tar.gz"
bin = "thing"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("runtime release-binary parses");
    match m.runtime.expect("has runtime") {
        RuntimeSpec::ReleaseBinary { asset, bin } => {
            assert_eq!(asset, "thing-${target}.tar.gz");
            assert_eq!(bin.as_deref(), Some("thing"));
        }
        other => panic!("expected release-binary runtime, got {other:?}"),
    }
}

#[test]
fn empty_runtime_command_is_rejected() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = []
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(matches!(err, ManifestError::Invalid(_)), "got {err:?}");
}

#[test]
fn empty_contribution_fields_are_rejected() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[[commands]]
id = ""

[[keybinds]]
command = ""
key = ""

[[ui]]
slot = "status-bar"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    let messages = match err {
        ManifestError::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(
        messages.iter().any(|m| m.contains("commands[0].id")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("keybinds[0].command")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("ui[0].id")),
        "{messages:?}"
    );
}

#[test]
fn unknown_ui_slot_is_a_parse_error() {
    // `slot` is a typed enum (closed set), so a slot this host does not render
    // is rejected at parse time, not carried forward like an unknown capability.
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[[ui]]
slot = "sidebar"
id = "panel"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(err, ManifestError::Parse(_)),
        "expected Parse, got {err:?}"
    );
}

#[test]
fn ui_slot_as_str_round_trips_the_wire_name() {
    // as_str (used for the install prompt / plugin info disclosure) must match
    // the kebab-case serde name a manifest declares.
    for (toml_slot, slot) in [
        ("status-bar", UiSlot::StatusBar),
        ("row-badge", UiSlot::RowBadge),
        ("pane", UiSlot::Pane),
        ("composer-action", UiSlot::ComposerAction),
        ("settings-page", UiSlot::SettingsPage),
        ("notification", UiSlot::Notification),
    ] {
        assert_eq!(slot.as_str(), toml_slot);
    }
}

#[test]
fn settings_page_requires_api_version_10() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 9

[[ui]]
slot = "settings-page"
id = "main"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        format!("{err:?}").contains("settings-page UI slots require api_version >= 10"),
        "{err:?}"
    );
}

#[test]
fn settings_page_parses_at_api_version_10() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 10

[[ui]]
slot = "settings-page"
id = "main"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("api 10 manifest parses");
    assert_eq!(m.ui[0].slot, UiSlot::SettingsPage);
    assert!(
        !UiSlot::SettingsPage.is_per_session(),
        "settings-page is global"
    );
}

#[test]
fn composer_action_requires_api_version_8() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 7

[[ui]]
slot = "composer-action"
id = "voice"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        format!("{err:?}").contains("composer-action UI slots require api_version >= 8"),
        "{err:?}"
    );
}

#[test]
fn empty_command_argument_is_rejected() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[runtime]
kind = "command"
command = [""]
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(matches!(err, ManifestError::Invalid(_)), "got {err:?}");
}

#[test]
fn unknown_fields_are_rejected() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2
frobnicate = true
"#;
    // A top-level key outside the schema is a hard parse error, not silently
    // ignored.
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(matches!(err, ManifestError::Parse(_)), "got {err:?}");
}

#[test]
fn empty_name_and_version_collect_all_problems() {
    let toml = r#"
id = "acme.thing"
name = ""
version = ""
api_version = 2
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    let messages = match err {
        ManifestError::Invalid(messages) => messages,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert!(messages.iter().any(|m| m.contains("name")), "{messages:?}");
    assert!(
        messages.iter().any(|m| m.contains("version")),
        "{messages:?}"
    );
}

#[test]
fn newer_api_version_reports_version_not_unknown_variant() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 9999
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(
            err,
            ManifestError::UnsupportedApiVersion { found: 9999, .. }
        ),
        "got {err:?}"
    );
}

#[test]
fn manifest_hash_is_stable_and_prefixed() {
    let bytes = b"id = \"acme.thing\"\n";
    let a = PluginManifest::hash_bytes(bytes);
    let b = PluginManifest::hash_bytes(bytes);
    assert_eq!(a, b);
    assert!(a.starts_with("sha256:"));
    assert_ne!(a, PluginManifest::hash_bytes(b"different"));
}

#[test]
fn screenshots_parse_and_round_trip() {
    let toml = r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 5

[[screenshots]]
path = "docs/screenshots/dashboard.png"
alt = "Dashboard card"
caption = "The plugin's dashboard card."

[[screenshots]]
path = "media/demo.gif"
alt = "Animated demo"
"#;
    let manifest = PluginManifest::from_toml_str(toml).expect("screenshots parse");
    assert_eq!(manifest.screenshots.len(), 2);
    assert_eq!(
        manifest.screenshots[0].path,
        "docs/screenshots/dashboard.png"
    );
    assert_eq!(
        manifest.screenshots[0].caption,
        "The plugin's dashboard card."
    );
    assert_eq!(manifest.screenshots[1].alt, "Animated demo");
    assert_eq!(manifest.screenshots[1].caption, "");
}

#[test]
fn screenshots_require_api_version_5() {
    let toml = r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 4

[[screenshots]]
path = "a.png"
alt = "a"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("screenshots require api_version >= 5"))),
        "got {err:?}"
    );
}

#[test]
fn screenshots_reject_absolute_url_and_traversal_paths() {
    for bad in [
        "https://tracker.example.com/x.png",
        "/etc/passwd.png",
        "../secrets.png",
        "a/../../b.png",
        "C:/Windows/x.png",
        "no-extension",
        "evil.svg",
    ] {
        let toml = format!(
            r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 5

[[screenshots]]
path = "{bad}"
alt = "x"
"#
        );
        let err = PluginManifest::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("must be a repository-relative image path"))),
            "path {bad:?} should be rejected, got {err:?}"
        );
    }
}

#[test]
fn screenshots_reject_empty_alt() {
    let toml = r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 5

[[screenshots]]
path = "a.png"
alt = "   "
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("alt must not be empty"))),
        "got {err:?}"
    );
}

#[test]
fn screenshots_reject_too_many() {
    let entries = (0..9)
        .map(|i| format!("[[screenshots]]\npath = \"s{i}.png\"\nalt = \"s{i}\"\n"))
        .collect::<String>();
    let toml = format!(
        "id = \"acme.widget\"\nname = \"Widget\"\nversion = \"0.1.0\"\napi_version = 5\n\n{entries}"
    );
    let err = PluginManifest::from_toml_str(&toml).unwrap_err();
    assert!(
        matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("at most 8 screenshots"))),
        "got {err:?}"
    );
}

#[test]
fn screenshot_path_ok_rejects_backslash_and_bad_shapes() {
    use aoe_plugin_api::screenshot_path_ok;
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
        assert!(!screenshot_path_ok(bad), "{bad:?} should be rejected");
    }
}

#[test]
fn icon_parses_and_round_trips() {
    let toml = r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 7
icon = "git-branch"
icon_asset = "assets/icon.png"
"#;
    let manifest = PluginManifest::from_toml_str(toml).expect("icon parses");
    assert_eq!(manifest.icon.as_deref(), Some("git-branch"));
    assert_eq!(manifest.icon_asset.as_deref(), Some("assets/icon.png"));
}

#[test]
fn icon_requires_api_version_7() {
    let toml = r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 6
icon = "git-branch"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("icon requires api_version >= 7"))),
        "got {err:?}"
    );
}

#[test]
fn icon_asset_requires_api_version_7() {
    let toml = r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 6
icon_asset = "assets/icon.png"
"#;
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("icon_asset requires api_version >= 7"))),
        "got {err:?}"
    );
}

#[test]
fn icon_asset_rejects_bad_paths() {
    for bad in [
        "https://tracker.example.com/x.png",
        "/etc/passwd.png",
        "../secrets.png",
        "evil.svg",
        "noext",
    ] {
        let toml = format!(
            r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 7
icon_asset = "{bad}"
"#
        );
        let err = PluginManifest::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("icon_asset") && e.contains("must be a repository-relative image path"))),
            "path {bad:?} should be rejected, got {err:?}"
        );
    }
}

#[test]
fn icon_rejects_bad_names() {
    for bad in ["GitHub", "git_branch", "-git", "git--branch", ""] {
        let toml = format!(
            r#"
id = "acme.widget"
name = "Widget"
version = "0.1.0"
api_version = 7
icon = "{bad}"
"#
        );
        let err = PluginManifest::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(&err, ManifestError::Invalid(errs) if errs.iter().any(|e| e.contains("must be a lucide kebab-case icon name"))),
            "icon {bad:?} should be rejected, got {err:?}"
        );
    }
}

#[test]
fn lucide_icon_name_ok_rejects_bad_shapes() {
    use aoe_plugin_api::lucide_icon_name_ok;
    assert!(lucide_icon_name_ok("git-branch"));
    assert!(lucide_icon_name_ok("puzzle"));
    for bad in ["GitHub", "git_branch", "-git", "git-", "git--branch", ""] {
        assert!(!lucide_icon_name_ok(bad), "{bad:?} should be rejected");
    }
}
