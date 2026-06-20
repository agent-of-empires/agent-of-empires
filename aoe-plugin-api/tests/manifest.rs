use aoe_plugin_api::{ManifestError, PluginManifest};

#[test]
fn minimal_manifest_parses_and_round_trips() {
    let toml = r#"
id = "aoe.web"
name = "Web Dashboard"
version = "1.0.0"
api_version = 1
description = "The aoe serve web dashboard."
"#;
    let manifest = PluginManifest::from_toml_str(toml).expect("valid manifest parses");
    assert_eq!(manifest.id.as_str(), "aoe.web");
    assert_eq!(manifest.name, "Web Dashboard");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.api_version, 1);

    let serialized = toml::to_string(&manifest).expect("serializes");
    let reparsed = PluginManifest::from_toml_str(&serialized).expect("round-trips");
    assert_eq!(reparsed.id.as_str(), "aoe.web");
}

#[test]
fn description_defaults_to_empty() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 1
"#;
    let manifest = PluginManifest::from_toml_str(toml).expect("description is optional");
    assert!(manifest.description.is_empty());
}

#[test]
fn unknown_fields_are_rejected() {
    let toml = r#"
id = "acme.thing"
name = "Thing"
version = "0.1.0"
api_version = 1
capabilities = ["pane-read"]
"#;
    // Contribution sections are not part of the core schema yet, so an
    // unrecognized key is a hard parse error rather than silently ignored.
    let err = PluginManifest::from_toml_str(toml).unwrap_err();
    assert!(matches!(err, ManifestError::Parse(_)), "got {err:?}");
}

#[test]
fn empty_name_and_version_collect_all_problems() {
    let toml = r#"
id = "acme.thing"
name = ""
version = ""
api_version = 1
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
