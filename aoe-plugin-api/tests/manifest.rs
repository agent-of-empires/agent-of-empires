use aoe_plugin_api::{ManifestError, PluginManifest, RuntimeSpec};

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
slot = "sidebar"
id = "panel"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("contribution sections parse");
    assert_eq!(m.capabilities.len(), 2);
    assert!(m.capabilities[0].is_known());
    assert_eq!(m.commands.len(), 1);
    assert_eq!(m.keybinds[0].key, "Ctrl+K");
    assert_eq!(m.settings[0].key, "endpoint");
    assert_eq!(m.ui[0].slot, "sidebar");
}

#[test]
fn deferred_sections_are_rejected() {
    // themes / status / panes are deferred until a consumer exists; with
    // deny_unknown_fields a manifest declaring one must fail to parse.
    for section in ["themes", "status", "panes"] {
        let toml = format!(
            r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 2

[[{section}]]
id = "x"
name = "x"
path = "x"
"#
        );
        let err = PluginManifest::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(err, ManifestError::Parse(_)),
            "[[{section}]] should be a hard parse error, got {err:?}"
        );
    }
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
"#;
    let m = PluginManifest::from_toml_str(toml).expect("runtime command parses");
    match m.runtime.expect("has runtime") {
        RuntimeSpec::Command { command } => assert_eq!(command, ["python3", "worker.py"]),
        other => panic!("expected command runtime, got {other:?}"),
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
slot = ""
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
        messages.iter().any(|m| m.contains("ui[0].slot")),
        "{messages:?}"
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
