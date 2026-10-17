//! Provider-native status and user-requested terminal replies, independent of ACP.

use serde::{Deserialize, Serialize};

/// Agent tools whose provider-native status and reply protocol this host implements.
pub const SUPPORTED_AGENT_INTEGRATION_TOOLS: &[&str] = &["omp"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationContribution {
    pub tool: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentIntegrationStatus {
    Running,
    Waiting,
    Idle,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationSnapshot {
    pub schema_version: u32,
    pub instance_id: String,
    pub provider_session_id: String,
    pub writer_pid: u32,
    pub revision: u64,
    pub status: AgentIntegrationStatus,
    /// Function-key names sent only to wake the provider extension after a user reply.
    pub reply_keys: Vec<String>,
    pub prompt: Option<AgentIntegrationPrompt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum AgentIntegrationPrompt {
    Approval {
        id: String,
        title: String,
        allow_always: bool,
    },
    Question {
        id: String,
        title: String,
        questions: Vec<AgentIntegrationQuestion>,
    },
}

impl AgentIntegrationPrompt {
    pub fn id(&self) -> &str {
        match self {
            Self::Approval { id, .. } | Self::Question { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AgentIntegrationOption>,
    #[serde(default)]
    pub multi: bool,
    pub recommended: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationAnswer {
    pub id: String,
    pub selected_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationApprovalChoice {
    Allow,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum AgentIntegrationResponse {
    Approval {
        choice: IntegrationApprovalChoice,
    },
    Question {
        answers: Vec<AgentIntegrationAnswer>,
    },
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIntegrationReply<'a> {
    pub schema_version: u32,
    pub instance_id: &'a str,
    pub provider_session_id: &'a str,
    pub writer_pid: u32,
    pub revision: u64,
    pub prompt_id: &'a str,
    pub response_id: String,
    pub response: AgentIntegrationResponse,
}
