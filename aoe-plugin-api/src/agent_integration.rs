//! Plugin-declared agent launch metadata.

use serde::{Deserialize, Serialize};

/// A plugin-provided agent launch command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationContribution {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}
