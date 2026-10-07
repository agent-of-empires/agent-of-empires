//! ACP capability-discovery DTOs for the `acp.capabilities.get` worker RPC

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_updated_at: Option<String>,
    pub models: Vec<AcpModelCapability>,
    pub modes: Vec<AcpModeCapability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<AcpThinkingCapability>,
}

/// A model choice the agent advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpModelCapability {
    pub id: String,
    pub display_name: String,
}

/// A reasoning-effort / thought-level choice the agent advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpThinkingCapability {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpModeCapability {
    pub id: String,
    pub display_name: String,
    pub approval_class: ApprovalClass,
}

/// Whether the host has ever observed this agent's advertised option catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    Undiscovered,
    Discovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClass {
    /// Approvals prompt a human through the host UI (adapter default).
    Interactive,
    Guarded,
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
                thinking: vec![AcpThinkingCapability {
                    id: "think".into(),
                    display_name: "Think".into(),
                }],
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
                    ],
                    "thinking": [{"id": "think", "display_name": "Think"}]
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
            thinking: vec![],
        };
        let json = serde_json::to_value(&agent).expect("serialize");
        assert!(json.get("catalog_updated_at").is_none());
        assert!(json.get("thinking").is_none());
        assert_eq!(json["catalog_status"], "undiscovered");
    }
}
