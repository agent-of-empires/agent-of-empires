//! ACP capability-discovery DTOs for the `acp.capabilities.get` worker RPC
//! (API v9, #2897).
//!
//! This is the stable wire contract a session-driving plugin (for example
//! `plugin-cron`) pins its fixtures against. The host assembles it from the
//! static agent registry plus the last option catalog each agent advertised;
//! answering never launches an agent, so a never-run agent reports
//! `CatalogStatus::Undiscovered` with empty model/mode lists. All lists are
//! sorted by id so serialized fixtures are deterministic. Fields are additive
//! from here on; an incompatible reshape bumps the crate `API_VERSION`.

use serde::{Deserialize, Serialize};

/// Response of `acp.capabilities.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpCapabilitiesResponse {
    pub agents: Vec<AcpAgentCapability>,
}

/// One agent the host can run in a structured session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpAgentCapability {
    /// Stable agent id, the value `sessions.create` accepts as `agent_id`.
    pub id: String,
    pub display_name: String,
    pub catalog_status: CatalogStatus,
    /// RFC3339 timestamp of the advertised catalog snapshot; `Some` only when
    /// `catalog_status` is `Discovered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_updated_at: Option<String>,
    pub models: Vec<AcpModelCapability>,
    pub modes: Vec<AcpModeCapability>,
}

/// A model choice the agent advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpModelCapability {
    pub id: String,
    pub display_name: String,
}

/// A permission/approval mode choice, carrying the HOST's security
/// classification. The plugin must not infer safety from mode names; the
/// host assigns `approval_class` and enforces it at `sessions.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpModeCapability {
    pub id: String,
    pub display_name: String,
    pub approval_class: ApprovalClass,
}

/// Whether the host has ever observed this agent's advertised option catalog.
/// Models/modes are populated only after the agent has run at least once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    Undiscovered,
    Discovered,
}

/// Host-assigned security class of an approval mode. `Unattended` requires
/// the distinct high-severity `session.unattended` grant at
/// `sessions.create`; the host classifies unknown modes as `Unattended`
/// (fail closed), never trusting a plugin- or agent-supplied label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClass {
    /// Approvals prompt a human through the host UI (adapter default).
    Interactive,
    /// A reviewed mode that preserves host approvals or prohibits mutation
    /// (for example a plan/read-only preset).
    Guarded,
    /// The agent can act without a human present (bypass or auto-write
    /// modes, and every mode the host cannot classify).
    Unattended,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_fixture_is_stable() {
        let response = AcpCapabilitiesResponse {
            agents: vec![AcpAgentCapability {
                id: "claude".into(),
                display_name: "Claude Code".into(),
                catalog_status: CatalogStatus::Discovered,
                catalog_updated_at: Some("2026-07-16T00:00:00Z".into()),
                models: vec![AcpModelCapability {
                    id: "sonnet".into(),
                    display_name: "Sonnet".into(),
                }],
                modes: vec![
                    AcpModeCapability {
                        id: "bypassPermissions".into(),
                        display_name: "Bypass Permissions".into(),
                        approval_class: ApprovalClass::Unattended,
                    },
                    AcpModeCapability {
                        id: "plan".into(),
                        display_name: "Plan".into(),
                        approval_class: ApprovalClass::Guarded,
                    },
                ],
            }],
        };
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "agents": [{
                    "id": "claude",
                    "display_name": "Claude Code",
                    "catalog_status": "discovered",
                    "catalog_updated_at": "2026-07-16T00:00:00Z",
                    "models": [{"id": "sonnet", "display_name": "Sonnet"}],
                    "modes": [
                        {"id": "bypassPermissions", "display_name": "Bypass Permissions", "approval_class": "unattended"},
                        {"id": "plan", "display_name": "Plan", "approval_class": "guarded"}
                    ]
                }]
            })
        );
        let round: AcpCapabilitiesResponse = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round, response);
    }

    #[test]
    fn undiscovered_omits_updated_at() {
        let agent = AcpAgentCapability {
            id: "codex".into(),
            display_name: "Codex".into(),
            catalog_status: CatalogStatus::Undiscovered,
            catalog_updated_at: None,
            models: vec![],
            modes: vec![],
        };
        let json = serde_json::to_value(&agent).expect("serialize");
        assert!(json.get("catalog_updated_at").is_none());
        assert_eq!(json["catalog_status"], "undiscovered");
    }
}
