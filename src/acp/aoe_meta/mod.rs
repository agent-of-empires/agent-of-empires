//! The `_meta.aoe` primitive contract. See
//! `docs/development/internals/acp-primitive-contract.md`.
//!
//! Core consumes only these generic primitives plus standard ACP; agent
//! specific interpretation lives outside core.

pub mod lease;
pub mod lint;
pub mod ordering;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Version of the `_meta.aoe` primitive wire shape.
pub const PRIMITIVE_SCHEMA_VERSION: u32 = 1;

/// Version of the proxy hello and process contract.
pub const PROXY_PROTOCOL_VERSION: u32 = 1;

/// Longest lease a single frame may grant.
pub const MAX_LEASE_TTL_MS: u64 = 60 * 60 * 1000;

type Meta = serde_json::Map<String, serde_json::Value>;

/// `_meta.aoe.proxy` on the initialize response, next to the adapter's own
/// unchanged `agent_info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProxyHello {
    #[schemars(regex(pattern = r"\S"))]
    pub plugin_id: String,
    #[schemars(range(min = 1, max = 1))]
    pub protocol_version: u32,
    #[schemars(range(min = 1, max = 1))]
    pub primitive_schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    #[error("malformed proxy hello: {0}")]
    Malformed(String),
    #[error("proxy hello has an empty plugin_id")]
    EmptyPluginId,
    #[error("unsupported proxy protocol_version {0} (core speaks {PROXY_PROTOCOL_VERSION})")]
    UnsupportedProtocol(u32),
    #[error("unsupported primitive_schema_version {0} (core speaks {PRIMITIVE_SCHEMA_VERSION})")]
    UnsupportedSchema(u32),
}

impl ProxyHello {
    /// `Ok(None)` when `meta` carries no hello: a plain ACP agent.
    pub fn from_meta(meta: &Meta) -> Result<Option<Self>, HelloError> {
        let Some(raw) = meta.get("aoe").and_then(|aoe| aoe.get("proxy")) else {
            return Ok(None);
        };
        let hello: Self = serde_json::from_value(raw.clone())
            .map_err(|e| HelloError::Malformed(e.to_string()))?;
        hello.validate()?;
        Ok(Some(hello))
    }

    pub fn validate(&self) -> Result<(), HelloError> {
        if self.plugin_id.trim().is_empty() {
            return Err(HelloError::EmptyPluginId);
        }
        if self.protocol_version != PROXY_PROTOCOL_VERSION {
            return Err(HelloError::UnsupportedProtocol(self.protocol_version));
        }
        if self.primitive_schema_version != PRIMITIVE_SCHEMA_VERSION {
            return Err(HelloError::UnsupportedSchema(self.primitive_schema_version));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    Busy,
    Task,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeaseVerb {
    #[default]
    Acquire,
    Renew,
    Release,
}

/// An expiring hold. `ttl_ms` is required for acquire and renew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseOp {
    pub id: String,
    pub kind: LeaseKind,
    #[serde(default, skip_serializing_if = "is_default")]
    pub op: LeaseVerb,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3_600_000))]
    pub ttl_ms: Option<u64>,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecallPrimitive {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesized_text: Option<String>,
}

/// Markdown whose bullet lines become the plan steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanProposal {
    pub markdown: String,
}

/// A card header hint. Validated and carried in v1; no surface renders it yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolPresentation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chips: Vec<String>,
}

/// Preview text for one option of a form elicitation. Validated and carried
/// in v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionPreview {
    pub option_id: String,
    pub markdown: String,
}

/// `_meta.aoe` on a `session/update` or tool call. `key` is the idempotency
/// key: a repeat within a session is dropped.
///
/// A frame with `after_adapter_sequence` is tailer-emitted: it is not an
/// adapter frame, and core delivers it only after adapter frame N.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    #[schemars(range(min = 1, max = 1))]
    pub v: u32,
    #[schemars(length(min = 1))]
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_adapter_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call: Option<String>,
    /// `true` drops the frame before the reducer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_recall: Option<MemoryRecallPrimitive>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_proposal: Option<PlanProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_presentation: Option<ToolPresentation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_preview: Option<OptionPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("malformed _meta.aoe: {0}")]
    Malformed(String),
    #[error("unsupported primitive schema version {0} (core speaks {PRIMITIVE_SCHEMA_VERSION})")]
    UnsupportedVersion(u32),
    #[error("_meta.aoe.key must not be empty")]
    EmptyKey,
    #[error("lease {0:?} needs a ttl_ms between 1 and {MAX_LEASE_TTL_MS}")]
    BadLeaseTtl(String),
}

impl Envelope {
    /// `Ok(None)` when `meta` carries no `aoe` object.
    pub fn from_meta(meta: &Meta) -> Result<Option<Self>, EnvelopeError> {
        let Some(raw) = meta.get("aoe") else {
            return Ok(None);
        };
        // A hello rides the initialize response, never an update.
        if raw.get("proxy").is_some() {
            return Err(EnvelopeError::Malformed(
                "proxy hello is only valid on the initialize response".into(),
            ));
        }
        let env: Self = serde_json::from_value(raw.clone())
            .map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
        env.validate()?;
        Ok(Some(env))
    }

    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.v != PRIMITIVE_SCHEMA_VERSION {
            return Err(EnvelopeError::UnsupportedVersion(self.v));
        }
        if self.key.is_empty() {
            return Err(EnvelopeError::EmptyKey);
        }
        if let Some(lease) = &self.lease {
            let needs_ttl = lease.op != LeaseVerb::Release;
            let ttl_ok = match lease.ttl_ms {
                Some(ttl) => (1..=MAX_LEASE_TTL_MS).contains(&ttl),
                None => !needs_ttl,
            };
            if !ttl_ok {
                return Err(EnvelopeError::BadLeaseTtl(lease.id.clone()));
            }
        }
        Ok(())
    }
}

/// The `_meta.aoe` of a serialized `session/update` update object.
pub fn from_update_value(update: &serde_json::Value) -> Result<Option<Envelope>, EnvelopeError> {
    match update.get("_meta").and_then(|m| m.as_object()) {
        Some(meta) => Envelope::from_meta(meta),
        None => Ok(None),
    }
}

/// Whether an update names an `after_adapter_sequence`, read without
/// validating the rest of the envelope: a tailer frame that fails validation
/// is still not an adapter frame.
pub fn is_anchored(update: &serde_json::Value) -> bool {
    update
        .pointer("/_meta/aoe/after_adapter_sequence")
        .is_some_and(|v| !v.is_null())
}

/// The envelope of an already-ingested update. A malformed one reads as
/// absent here: `Shared` rejects it before mapping.
pub fn envelope_of(meta: &Option<Meta>) -> Option<Envelope> {
    meta.as_ref()
        .and_then(|m| Envelope::from_meta(m).ok().flatten())
}

/// The published schema: `proxy` is the hello, `primitive` an update's
/// `_meta.aoe`.
#[derive(JsonSchema)]
pub struct Contract {
    pub proxy: ProxyHello,
    pub primitive: Envelope,
}

pub fn schema_json() -> String {
    let schema = schemars::schema_for!(Contract);
    let mut out = serde_json::to_string_pretty(&schema).expect("schema serializes");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(v: serde_json::Value) -> Meta {
        v.as_object().expect("object").clone()
    }

    #[test]
    fn hello_cases() {
        // (name, meta, expected)
        let cases = [
            ("absent", json!({}), Ok(None)),
            (
                "valid",
                json!({"aoe": {"proxy": {"plugin_id": "p.claude", "protocol_version": 1, "primitive_schema_version": 1}}}),
                Ok(Some("p.claude")),
            ),
            (
                "empty id",
                json!({"aoe": {"proxy": {"plugin_id": " ", "protocol_version": 1, "primitive_schema_version": 1}}}),
                Err("empty"),
            ),
            (
                "future protocol",
                json!({"aoe": {"proxy": {"plugin_id": "p", "protocol_version": 2, "primitive_schema_version": 1}}}),
                Err("protocol_version"),
            ),
            (
                "future schema",
                json!({"aoe": {"proxy": {"plugin_id": "p", "protocol_version": 1, "primitive_schema_version": 9}}}),
                Err("primitive_schema_version"),
            ),
            (
                "unknown field",
                json!({"aoe": {"proxy": {"plugin_id": "p", "protocol_version": 1, "primitive_schema_version": 1, "x": 1}}}),
                Err("malformed"),
            ),
        ];
        for (name, m, expected) in cases {
            let got = ProxyHello::from_meta(&meta(m));
            match expected {
                Ok(id) => assert_eq!(
                    got.unwrap().map(|h| h.plugin_id),
                    id.map(String::from),
                    "{name}"
                ),
                Err(needle) => {
                    let err = got.unwrap_err().to_string();
                    assert!(err.contains(needle), "{name}: {err}");
                }
            }
        }
    }

    #[test]
    fn envelope_cases() {
        // (name, aoe object, error substring or None)
        let cases = [
            ("minimal", json!({"v": 1, "key": "k"}), None),
            (
                "wrong version",
                json!({"v": 2, "key": "k"}),
                Some("version"),
            ),
            ("empty key", json!({"v": 1, "key": ""}), Some("key")),
            (
                "unknown field",
                json!({"v": 1, "key": "k", "nope": 1}),
                Some("malformed"),
            ),
            (
                "acquire needs ttl",
                json!({"v": 1, "key": "k", "lease": {"id": "a", "kind": "busy"}}),
                Some("ttl_ms"),
            ),
            (
                "ttl over cap",
                json!({"v": 1, "key": "k", "lease": {"id": "a", "kind": "busy", "ttl_ms": MAX_LEASE_TTL_MS + 1}}),
                Some("ttl_ms"),
            ),
            (
                "release without ttl",
                json!({"v": 1, "key": "k", "lease": {"id": "a", "kind": "task", "op": "release"}}),
                None,
            ),
            (
                "hello on an update",
                json!({"v": 1, "key": "k", "proxy": {}}),
                Some("initialize"),
            ),
        ];
        for (name, aoe, expected) in cases {
            let got = Envelope::from_meta(&meta(json!({ "aoe": aoe })));
            match expected {
                None => assert!(got.unwrap().is_some(), "{name}"),
                Some(needle) => {
                    let err = got.unwrap_err().to_string();
                    assert!(err.contains(needle), "{name}: {err}");
                }
            }
        }
    }

    #[test]
    fn published_schema_is_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/aoe-meta-v1.schema.json"
        );
        let on_disk = std::fs::read_to_string(path).expect("schema file present");
        assert_eq!(on_disk, schema_json(), "run `cargo xtask gen-meta-schema`");
    }

    /// Every constraint the runtime enforces that JSON Schema can express. A
    /// lease acquire or renew without `ttl_ms` is a runtime rule only.
    #[test]
    fn published_schema_agrees_with_the_types() {
        let schema: serde_json::Value = serde_json::from_str(&schema_json()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let hello = json!({"plugin_id": "p", "protocol_version": 1, "primitive_schema_version": 1});
        let lease = |ttl: u64| json!({"id": "a", "kind": "busy", "ttl_ms": ttl});
        let with = |primitive: serde_json::Value| json!({"proxy": hello, "primitive": primitive});
        // (name, document, valid)
        let cases = [
            ("minimal", with(json!({"v": 1, "key": "k"})), true),
            (
                "ephemeral lease",
                with(json!({"v": 1, "key": "k", "ephemeral": true, "lease": lease(5000)})),
                true,
            ),
            ("missing key", with(json!({"v": 1})), false),
            ("future version", with(json!({"v": 2, "key": "k"})), false),
            ("empty key", with(json!({"v": 1, "key": ""})), false),
            (
                "zero ttl",
                with(json!({"v": 1, "key": "k", "lease": lease(0)})),
                false,
            ),
            (
                "ttl over the cap",
                with(json!({"v": 1, "key": "k", "lease": lease(MAX_LEASE_TTL_MS + 1)})),
                false,
            ),
            (
                "blank plugin id",
                json!({"proxy": {"plugin_id": "  ", "protocol_version": 1, "primitive_schema_version": 1},
                       "primitive": {"v": 1, "key": "k"}}),
                false,
            ),
            (
                "future protocol",
                json!({"proxy": {"plugin_id": "p", "protocol_version": 2, "primitive_schema_version": 1},
                       "primitive": {"v": 1, "key": "k"}}),
                false,
            ),
        ];
        for (name, document, valid) in cases {
            assert_eq!(validator.is_valid(&document), valid, "{name}");
        }
    }
}
