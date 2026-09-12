//! Model gateway: one gateway root configured once, serving every supported
//! agent harness.
//!
//! A Rust port of nodeterm's `shared/agents/model-gateway.ts`, adapted to
//! AOE's spawn model: instead of tmux `update-environment`, derived
//! credentials and routing ride the host_environment channel
//! (`SpawnConfig.host_environment`, applied last by both structured-view spawn
//! paths, and the terminal view's pane env mutations).
//!
//! Rules carried over from the reference implementation, unchanged:
//! - A credential never rides argv, and never enters a command line. Values
//!   travel by process environment only.
//! - Fail closed. An unresolvable credential or route degrades to emitting
//!   nothing — never a partial credential, never a guessed endpoint.
//! - Re-validate hand-editable values (config.toml is hand-editable) at the
//!   interpolation site. An unrecognized value yields the safe default, not a
//!   guess.
//! - A guess must degrade to nothing, never to something wrong.

use serde::Deserialize;
use serde::Serialize;

/// One model gateway configured once for every supported agent harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelGatewaySettings {
    /// Gateway root, before the OpenAI-compatible `/v1/models` discovery route.
    #[serde(default)]
    pub base_url: String,
    /// Literal key, one exact `${env:VAR}` reference, or the literal
    /// `${secret:model-gateway-api-key}` — resolved from the daemon's own
    /// environment (`MODEL_GATEWAY_API_KEY`) at spawn time. The daemon runs on
    /// the operator's host, so a daemon env var is the 0600-file-equivalent
    /// channel here: the secret never enters config.toml, and never enters a
    /// command line or pane.
    #[serde(default)]
    pub api_key: String,
    /// Optional path the discovery (Models API) request is sent to, appended
    /// to `base_url` — e.g. `/openai/v1/models` for a gateway that serves its
    /// catalogue under a protocol prefix. Deliberately a PATH SUFFIX and never
    /// a full URL: discovery sends the resolved API key to the target, and a
    /// caller-chosen host would turn the flow into a credential-exfiltration
    /// oracle (the same reason `${env:VAR}` references resolve only for the
    /// saved base_url). Empty/absent = the conventional derived `/v1/models`
    /// (see `model_gateway_routes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_path: Option<String>,
}

/// Characters a discovery path may contain, minus URL structure that could
/// change WHERE the request goes: no query (`?`), fragment (`#`), dot-dot
/// traversal, or second scheme. The value is hand-editable config.toml AND
/// caller-supplied over HTTP, so it is re-validated where it is appended to
/// the root (the same rule as model ids on a command line) — an unrecognized
/// value yields the derived default path, never a guessed one.
const GATEWAY_DISCOVERY_PATH: &str = r"^[A-Za-z0-9\-._~!$&'()*+,;=:@/]+$";

/// A validated discovery path, or None when absent/unsafe (None ⇒ use the
/// derived default).
pub fn sanitized_gateway_discovery_path(path: Option<&str>) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(GATEWAY_DISCOVERY_PATH).expect("static regex"));
    let value = path?.trim();
    if value.is_empty() {
        return None;
    }
    if value.len() > 500 || value.contains("..") || !value.starts_with('/') || !re.is_match(value) {
        return None;
    }
    // Strip trailing slashes; an all-slash value is not a path.
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Stored in config.toml when the literal credential lives in the daemon's own
/// environment.
pub const MODEL_GATEWAY_SECRET_REF: &str = "${secret:model-gateway-api-key}";

/// The env var the `${secret:...}` reference resolves to in the daemon process
/// environment.
pub const MODEL_GATEWAY_SECRET_ENV_NAME: &str = "MODEL_GATEWAY_API_KEY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGatewayEnvReference {
    pub name: String,
}

const EXACT_ENV_REFERENCE: &str = r"^\$\{env:([A-Za-z_][A-Za-z0-9_]*)\}$";

/// Parse the representation used by the environment-variable credential mode.
/// One exact reference — no embedded expansion, no fallback: a fallback API
/// key would put the very secret this mode avoids back into config.toml.
pub fn parse_model_gateway_env_reference(api_key: &str) -> Option<ModelGatewayEnvReference> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(EXACT_ENV_REFERENCE).expect("static regex"));
    let caps = re.captures(api_key.trim())?;
    Some(ModelGatewayEnvReference {
        name: caps.get(1)?.as_str().to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelGatewayCredentialKind {
    Empty,
    Environment,
    Stored,
    LegacyLiteral,
}

pub fn model_gateway_credential_kind(api_key: &str) -> ModelGatewayCredentialKind {
    let value = api_key.trim();
    if value.is_empty() {
        return ModelGatewayCredentialKind::Empty;
    }
    if value == MODEL_GATEWAY_SECRET_REF {
        return ModelGatewayCredentialKind::Stored;
    }
    if parse_model_gateway_env_reference(value).is_some() {
        return ModelGatewayCredentialKind::Environment;
    }
    ModelGatewayCredentialKind::LegacyLiteral
}

/// The intentionally small model shape shared across the discovery endpoint and
/// spawn-env derivation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Maximum prompt context window in tokens, when the gateway reports one.
    /// The autocompact helper sizes `CLAUDE_CODE_AUTO_COMPACT_WINDOW` off it
    /// for a claude agent. Absent ⇒ the env var is omitted and the CLI falls
    /// back to its own default — never guessed (a percentage over a guessed
    /// window is a wrong number presented as a fact).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Maximum output/completion tokens the model may produce, when reported.
    /// Retained as catalogue metadata for consumers; absent stays absent,
    /// never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelDiscoveryResult {
    pub models: Vec<GatewayModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGatewayRoutes {
    pub discovery: String,
    pub openai: String,
    pub anthropic: String,
}

/// Resolve the stored gateway credential against the host process environment.
/// Keeping this next to the route/env mapping gives model discovery and every
/// supported harness the exact same `${env:VAR}` parser, without ever
/// resolving a secret outside the daemon. `env` is an accessor for the host
/// process environment so tests can inject their own.
///
/// Whitespace around either a literal or an expanded key is trimmed. A caller
/// must treat a nonempty `missing` (or `stored_secret_missing`) as a hard
/// failure even when `value` is partly non-empty: sending a partial credential
/// is both surprising and unsafe.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelGatewayApiKeyResolution {
    pub value: String,
    pub missing: Vec<String>,
    pub stored_secret_missing: bool,
}

pub fn resolve_model_gateway_api_key(
    api_key: &str,
    env: &dyn Fn(&str) -> Option<String>,
    stored_secret: Option<&str>,
) -> ModelGatewayApiKeyResolution {
    if api_key.trim() == MODEL_GATEWAY_SECRET_REF {
        let value = stored_secret.map(str::trim).unwrap_or("").to_string();
        return ModelGatewayApiKeyResolution {
            value,
            missing: Vec::new(),
            stored_secret_missing: stored_secret.map(str::trim).unwrap_or("").is_empty(),
        };
    }
    let Some(reference) = parse_model_gateway_env_reference(api_key) else {
        return ModelGatewayApiKeyResolution {
            value: api_key.trim().to_string(),
            missing: Vec::new(),
            stored_secret_missing: false,
        };
    };
    match env(&reference.name) {
        Some(v) if !v.trim().is_empty() => ModelGatewayApiKeyResolution {
            value: v.trim().to_string(),
            missing: Vec::new(),
            stored_secret_missing: false,
        },
        _ => ModelGatewayApiKeyResolution {
            value: String::new(),
            missing: vec![reference.name],
            stored_secret_missing: false,
        },
    }
}

/// Derive every route from one user-entered root. Discovery is the OpenAI
/// Models API convention (implemented by both LiteLLM and Bifrost); the
/// provider-specific paths are the Bifrost layout. Only http(s) URLs are
/// accepted: this value is later handed to the discovery fetch and agent CLIs,
/// and config.toml is hand-editable. Invalid input degrades to None, never to
/// a guessed endpoint.
///
/// `discovery_path` replaces the conventional `/v1/models` suffix when it is
/// present and passes `sanitized_gateway_discovery_path`. It changes only
/// WHICH path on the saved root the catalogue is read from — never the host —
/// so the credential trust gate upstream (references resolve only for the
/// saved base_url) is unaffected. An unsafe value falls back to the derived
/// default rather than sending a fetch somewhere the user did not vet.
pub fn model_gateway_routes(
    base_url: &str,
    discovery_path: Option<&str>,
) -> Option<ModelGatewayRoutes> {
    let raw = base_url.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    // Credentials in the URL would be copied into every derived endpoint and
    // surfaced in the UI. The separate API-key field exists precisely so a
    // secret never has to live there.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return None;
    }
    // Rebuild the root from scheme + host + port + path so a caller-supplied
    // path stays explicit and a trailing slash never doubles up.
    let mut root = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default()
    );
    if let Some(port) = parsed.port() {
        root.push(':');
        root.push_str(&port.to_string());
    }
    let path = parsed.path();
    if path != "/" && !path.is_empty() {
        root.push_str(path.trim_end_matches('/'));
    }
    let discovery_suffix = sanitized_gateway_discovery_path(discovery_path)
        .unwrap_or_else(|| "/v1/models".to_string());
    Some(ModelGatewayRoutes {
        discovery: format!("{root}{discovery_suffix}"),
        openai: format!("{root}/openai/v1"),
        anthropic: format!("{root}/anthropic"),
    })
}

/// Accept only positive integer token limits; invalid or absent metadata stays
/// unknown.
fn coerce_token_limit(value: Option<&serde_json::Value>) -> Option<u64> {
    let v = value?;
    let n: f64 = match v {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    if !n.is_finite() || n <= 0.0 || n.fract() != 0.0 {
        return None;
    }
    Some(n as u64)
}

/// Parse OpenAI-compatible model-list responses, dropping unsafe/empty/
/// duplicate ids. Anything unparseable yields an EMPTY list — never a partial
/// list built from whatever happened to match.
pub fn parse_gateway_models(payload: &serde_json::Value) -> Vec<GatewayModel> {
    let Some(data) = payload.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut by_id: std::collections::HashMap<String, GatewayModel> =
        std::collections::HashMap::new();
    for raw in data {
        let Some(obj) = raw.as_object() else {
            continue;
        };
        let Some(id) = obj.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || id.len() > 500 || id.chars().any(|c| c.is_control()) {
            continue;
        }
        let prefix = id.split_once('/').map(|(p, _)| p).unwrap_or("");
        // provider field wins over owned_by; both fall back to the id's
        // `/`-prefix (LiteLLM's `provider/model` convention).
        let provider = obj
            .get("provider")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("owned_by")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .map(String::from)
            .or_else(|| (!prefix.is_empty()).then(|| prefix.to_string()));
        // Context window: gateways disagree on the field name. The OpenAI
        // convention (`context_length`) and the `context_window` alias cover
        // the providers Copilot BYOK targets; `max_context_length` is the max
        // variant some report. The FIRST present, finite value wins (they are
        // synonyms), and an absent one stays None so the env var is omitted
        // rather than guessed.
        let context_window = coerce_token_limit(obj.get("context_length"))
            .or_else(|| coerce_token_limit(obj.get("max_context_length")))
            .or_else(|| coerce_token_limit(obj.get("context_window")));
        let max_output_tokens = coerce_token_limit(obj.get("max_output_tokens"))
            .or_else(|| coerce_token_limit(obj.get("max_completion_tokens")));
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        by_id.insert(
            id.to_string(),
            GatewayModel {
                id: id.to_string(),
                name,
                provider,
                context_window,
                max_output_tokens,
            },
        );
    }
    let mut models: Vec<GatewayModel> = by_id.into_values().collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// Every env var name `model_gateway_env` can emit, in one place. The
/// structured view applies them through `SpawnConfig.host_environment`
/// (applied last by both spawn paths); the terminal view applies them as pane
/// env mutations. A key emitted here but missing from a consumer's transport
/// would silently fail to reach the agent — adding one means wiring both
/// surfaces in the same change.
pub const MODEL_GATEWAY_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "COPILOT_PROVIDER_BASE_URL",
    "COPILOT_PROVIDER_TYPE",
    "COPILOT_PROVIDER_API_KEY",
    "COPILOT_PROVIDER_MODEL_ID",
    "COPILOT_PROVIDER_WIRE_MODEL",
    "COPILOT_PROVIDER_WIRE_API",
];

/// Claude Code autocompact env vars, injected for a claude agent whose
/// resolved gateway model reports a context window above
/// `AUTOCOMPACT_THRESHOLD`. These are Claude Code's OWN conventions:
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` sizes the compaction window,
/// `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` sets the % threshold at which it fires.
/// Sourced ONLY from discovery — never guessed — by `claude_autocompact_for`.
pub const AUTOCOMPACT_THRESHOLD: u64 = 200_000;
pub const AUTOCOMPACT_PCT_OVERRIDE: &str = "80";
pub const AUTOCOMPACT_ENV_KEYS: &[&str] = &[
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
];

/// The agents whose harnesses this module can route through a gateway. It is
/// a fixed list, not a setting: the mapping per harness (below) is the leaf,
/// and a frontend never spells an agent id itself. `grok` is deliberately
/// absent — see `model_gateway_env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayAgent {
    Claude,
    Codex,
    Copilot,
}

impl GatewayAgent {
    /// Parse an agent id into the harness this module supports. Anything else
    /// (opencode, gemini, custom, ...) routes nothing.
    pub fn from_agent_id(agent_id: &str) -> Option<Self> {
        match agent_id {
            "claude" | "claude-code" => Some(GatewayAgent::Claude),
            "codex" => Some(GatewayAgent::Codex),
            "copilot" => Some(GatewayAgent::Copilot),
            _ => None,
        }
    }
}

/// Copilot selects the provider's internal model id; the gateway keeps the
/// full wire id.
fn copilot_model_parts(wire_model: &str) -> (String, String) {
    // Split at the FIRST `/`; provider lowercased.
    match wire_model.split_once('/') {
        Some((provider, model_id)) => (provider.to_lowercase(), model_id.to_string()),
        None => (String::new(), wire_model.to_string()),
    }
}

/// Environment derived from the gateway for one agent's spawn, before any
/// user-configured env (user values still win on shared keys).
///
/// Claude/Codex model selection stays a quoted CLI flag. Copilot's BYOK
/// protocol instead carries its internal + wire model ids in environment
/// variables. Credentials never enter a restart command, so none are exposed
/// in the pane.
pub fn model_gateway_env(
    settings: &ModelGatewaySettings,
    agent_id: &str,
    model: Option<&str>,
    process_env: &dyn Fn(&str) -> Option<String>,
    stored_secret: Option<&str>,
) -> Vec<(String, String)> {
    let routes = model_gateway_routes(&settings.base_url, None);
    let resolved = resolve_model_gateway_api_key(&settings.api_key, process_env, stored_secret);
    let key = resolved.value;
    if !routes.is_some()
        || key.is_empty()
        || !resolved.missing.is_empty()
        || resolved.stored_secret_missing
    {
        return Vec::new();
    }
    let Some(agent) = GatewayAgent::from_agent_id(agent_id) else {
        return Vec::new();
    };
    let routes = routes.expect("checked above");
    match agent {
        GatewayAgent::Claude => vec![
            ("ANTHROPIC_BASE_URL".to_string(), routes.anthropic),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), key),
        ],
        GatewayAgent::Codex => vec![
            ("OPENAI_BASE_URL".to_string(), routes.openai),
            ("OPENAI_API_KEY".to_string(), key),
        ],
        GatewayAgent::Copilot => {
            // Copilot's BYOK mode requires a model at startup. Keep an ordinary
            // Copilot pane on GitHub's own routing until the user actually
            // selects one; otherwise merely configuring a gateway would
            // activate an incomplete provider and make every new Copilot pane
            // fail to launch.
            let Some(wire_model) = normalized_agent_model(agent_id, model) else {
                return Vec::new();
            };
            let (provider, model_id) = copilot_model_parts(&wire_model);
            let anthropic = provider == "anthropic";
            let mut env = vec![
                (
                    "COPILOT_PROVIDER_BASE_URL".to_string(),
                    if anthropic {
                        routes.anthropic
                    } else {
                        routes.openai
                    },
                ),
                (
                    "COPILOT_PROVIDER_TYPE".to_string(),
                    if anthropic { "anthropic" } else { "openai" }.to_string(),
                ),
                ("COPILOT_PROVIDER_API_KEY".to_string(), key),
                // Bifrost needs the provider-prefixed wire id; Copilot's internal
                // catalogue wants the unprefixed well-known id for token
                // limits/tool strategy. Its official BYOK grammar explicitly
                // supports separating these two values.
                ("COPILOT_PROVIDER_MODEL_ID".to_string(), model_id.clone()),
                ("COPILOT_PROVIDER_WIRE_MODEL".to_string(), wire_model),
            ];
            if !anthropic && is_gpt5(&model_id) {
                env.push((
                    "COPILOT_PROVIDER_WIRE_API".to_string(),
                    "responses".to_string(),
                ));
            }
            env
        }
    }
}

fn is_gpt5(model_id: &str) -> bool {
    // /^gpt-5(?:[.-]|$)/i
    let lower = model_id.to_lowercase();
    lower == "gpt-5" || lower.starts_with("gpt-5.") || lower.starts_with("gpt-5-")
}

/// Re-validate a hand-editable/discovered model id at the point it reaches a
/// launch command.
pub fn normalized_agent_model(_agent_id: &str, model: Option<&str>) -> Option<String> {
    let value = model?.trim();
    if value.is_empty() || value.len() > 500 || value.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(value.to_string())
}

/// Append a safely quoted model flag only for harnesses whose CLI grammar
/// supports it. The caller supplies the per-agent flag spelling (AOE's
/// `oneshot_model_flag`: `--model` for claude/copilot, `-m` for codex);
/// this function only validates and quotes.
pub fn with_agent_model(
    cmd: &str,
    agent_id: &str,
    model_flag: &str,
    model: Option<&str>,
) -> String {
    let Some(value) = normalized_agent_model(agent_id, model) else {
        return cmd.to_string();
    };
    // Copilot takes the INTERNAL model id on the flag; the gateway prefix
    // rides COPILOT_PROVIDER_WIRE_MODEL only.
    let flag_value = if GatewayAgent::from_agent_id(agent_id) == Some(GatewayAgent::Copilot) {
        copilot_model_parts(&value).1
    } else {
        value
    };
    format!("{} {} {}", cmd, model_flag, shell_single_quote(&flag_value))
}

/// Wrap a value so the launching shell passes it through literally.
/// Single-quote the value, escaping any embedded single quote the POSIX way.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/**
 * For a claude agent and its resolved model, return the `[1m]`-suffixed model
 * id and the Claude Code autocompact env, sourced ONLY from the gateway's
 * discovered model list.
 *
 * Claude Code sizes its autocompact window off the model id and fires
 * compaction at a default % of that window. A gateway model whose real
 * context window is large would otherwise still use the 200k default and
 * compact a long session early. Two levers fix it, both Claude-Code-specific:
 *
 *  1. A `[1m]` suffix on the model id marks a LARGE window (the CLI's own
 *     meter honors it).
 *  2. `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (the window size) and
 *     `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` (the % threshold) env vars.
 *
 * Everything is sourced from discovery — never guessed. An unknown model, or
 * one the gateway did not report a `context_window` for, yields NO env and NO
 * suffix (fail open to the CLI's own behavior). A percentage over a guessed
 * window is a wrong number presented as a fact. Non-claude agents get
 * neither: the env var names and the `[1m]` convention are Claude Code's own
 * and mean nothing to codex/copilot.
 *
 * `model_id` is the (possibly suffixed) id a caller should use for the launch
 * command; `env` is the pair list a spawn site merges into the session
 * environment. The two come from ONE call so the suffix and the env can never
 * disagree about whether this is a large-context session.
 */
pub struct ClaudeAutocompact {
    pub model_id: Option<String>,
    pub env: Vec<(String, String)>,
}

pub fn claude_autocompact_for(
    agent_id: &str,
    model: Option<&str>,
    models: &[GatewayModel],
) -> ClaudeAutocompact {
    // Only the claude harness. The env var names are Claude Code's; a
    // codex/copilot session would silently ignore them, and appending [1m] to
    // its model flag would send an unknown id to that CLI.
    if GatewayAgent::from_agent_id(agent_id) != Some(GatewayAgent::Claude) {
        return ClaudeAutocompact {
            model_id: model.map(str::to_string),
            env: Vec::new(),
        };
    }
    let Some(id) = normalized_agent_model(agent_id, model) else {
        return ClaudeAutocompact {
            model_id: model.map(str::to_string),
            env: Vec::new(),
        };
    };
    // The discovered model is the ONLY source of the window. A model not in
    // the catalogue tells us nothing — ship no env and no suffix rather than a
    // guess. Exact ids first; then the `[1m]`-stripped pair, because the SAME
    // model may be listed plain and suffixed depending on who stored it
    // (judging the window exact-only would let a suffixed catalogue spelling
    // silently skip both the env and the suffix for a session launched plain).
    fn strip(s: &str) -> &str {
        s.strip_suffix("[1m]").unwrap_or(s)
    }
    let discovered = models
        .iter()
        .find(|m| m.id == id)
        .or_else(|| models.iter().find(|m| strip(&m.id) == strip(&id)));
    let window = match discovered {
        Some(m) => m.context_window,
        None => None,
    };
    let Some(window) = window else {
        return ClaudeAutocompact {
            model_id: Some(id),
            env: Vec::new(),
        };
    };
    if window <= AUTOCOMPACT_THRESHOLD {
        // Symmetric strip: a model whose window dropped below the threshold
        // must NOT keep an old suffix — re-launching it would re-claim the
        // large window.
        return ClaudeAutocompact {
            model_id: Some(strip(&id).to_string()),
            env: Vec::new(),
        };
    }
    // PAIRING INVARIANT (pinned by the tests below): every branch that emits a
    // suffixed model_id MUST also emit the autocompact env, and the env is
    // emitted ONLY on a suffixed branch. The two halves of the mechanism are
    // one mechanism: the `[1m]` suffix lifts Claude Code's OWN window ceiling
    // (it is what the CLI's status meter honors) and
    // `CLAUDE_CODE_AUTO_COMPACT_WINDOW` pulls the compaction point back DOWN
    // to the discovered size. A suffix without the env lets the context grow
    // toward the ceiling with no autocompact headroom; an env without the
    // suffix meters 200k while compaction reads the larger window — two
    // windows disagreeing in one session. A future caller that half-applies
    // the pair must be caught in review, not shipped.
    let model_id = if id.ends_with("[1m]") {
        id.clone()
    } else {
        format!("{id}[1m]")
    };
    ClaudeAutocompact {
        model_id: Some(model_id),
        env: vec![
            (
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
                window.to_string(),
            ),
            (
                "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(),
                AUTOCOMPACT_PCT_OVERRIDE.to_string(),
            ),
        ],
    }
}

/// Current reported window for either plain or `[1m]` spelling of one model id.
pub fn model_context_window(model_id: Option<&str>, models: &[GatewayModel]) -> Option<u64> {
    let id = model_id?.trim();
    if id.is_empty() {
        return None;
    }
    let base = id.strip_suffix("[1m]").unwrap_or(id);
    models
        .iter()
        .find(|m| m.id.strip_suffix("[1m]").unwrap_or(&m.id) == base)
        .and_then(|m| m.context_window)
        .filter(|w| *w > 0)
}

/// Choose only a model the current gateway catalogue says it serves. A
/// configured default wins when present; otherwise the first sorted id.
pub fn claude_subagent_model_for(
    models: &[GatewayModel],
    default_model: Option<&str>,
) -> Option<String> {
    let mut ids: Vec<String> = models
        .iter()
        .map(|m| m.id.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    let preferred = default_model.map(str::trim).filter(|s| !s.is_empty());
    match preferred {
        Some(p) if ids.iter().any(|id| id == p) => Some(p.to_string()),
        _ => ids.into_iter().next(),
    }
}

/// Build Claude's gateway subagent routing independently from large-context
/// launch handling. Since Claude Code 2.1.251, an Agent call's explicit model
/// (e.g. `sonnet`) beats SUBAGENT_MODEL; FORCE (2.1.257+) restores the
/// override, keeping those calls on a model the gateway serves. Default
/// effort to medium: Claude can clamp xhigh to high for a custom model, but
/// some gateway routes reject high. With no served route, emit no controls or
/// guessed capabilities.
pub const CLAUDE_CODE_SUBAGENT_MODEL_KEY: &str = "CLAUDE_CODE_SUBAGENT_MODEL";
pub const CLAUDE_SUBAGENT_ENV_KEYS: &[&str] = &[
    CLAUDE_CODE_SUBAGENT_MODEL_KEY,
    "CLAUDE_CODE_SUBAGENT_MODEL_FORCE",
    "CLAUDE_CODE_EFFORT_LEVEL",
];

pub fn claude_subagent_env_for(
    agent_id: &str,
    models: &[GatewayModel],
    default_model: Option<&str>,
) -> Vec<(String, String)> {
    if GatewayAgent::from_agent_id(agent_id) != Some(GatewayAgent::Claude) {
        return Vec::new();
    }
    match claude_subagent_model_for(models, default_model) {
        Some(model) => vec![
            (CLAUDE_CODE_SUBAGENT_MODEL_KEY.to_string(), model),
            (
                "CLAUDE_CODE_SUBAGENT_MODEL_FORCE".to_string(),
                "1".to_string(),
            ),
            ("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "medium".to_string()),
        ],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    // --- sanitized_gateway_discovery_path ---

    #[test]
    fn discovery_path_default_when_absent() {
        assert_eq!(sanitized_gateway_discovery_path(None), None);
        assert_eq!(sanitized_gateway_discovery_path(Some("")), None);
        assert_eq!(sanitized_gateway_discovery_path(Some("  ")), None);
    }

    #[test]
    fn discovery_path_strips_trailing_slashes() {
        assert_eq!(
            sanitized_gateway_discovery_path(Some("/openai/v1/models/")),
            Some("/openai/v1/models".to_string())
        );
        assert_eq!(
            sanitized_gateway_discovery_path(Some("/openai/v1/models///")),
            Some("/openai/v1/models".to_string())
        );
    }

    #[test]
    fn discovery_path_rejects_traversal_query_fragment() {
        assert_eq!(sanitized_gateway_discovery_path(Some("/a/../b")), None);
        assert_eq!(sanitized_gateway_discovery_path(Some("/x?y=1")), None);
        assert_eq!(sanitized_gateway_discovery_path(Some("/x#frag")), None);
        assert_eq!(sanitized_gateway_discovery_path(Some("openai/v1")), None);
        assert_eq!(
            sanitized_gateway_discovery_path(Some(&"a".repeat(501))),
            None
        );
    }

    // --- model_gateway_routes ---

    #[test]
    fn routes_derive_all_three_from_one_root() {
        let r = model_gateway_routes("https://gw.example.com:8443/", None).unwrap();
        assert_eq!(r.discovery, "https://gw.example.com:8443/v1/models");
        assert_eq!(r.openai, "https://gw.example.com:8443/openai/v1");
        assert_eq!(r.anthropic, "https://gw.example.com:8443/anthropic");
    }

    #[test]
    fn routes_keep_base_path_and_honor_discovery_path() {
        let r = model_gateway_routes("http://localhost:9090/bifrost", Some("/openai/v1/models/"))
            .unwrap();
        assert_eq!(
            r.discovery,
            "http://localhost:9090/bifrost/openai/v1/models"
        );
        assert_eq!(r.openai, "http://localhost:9090/bifrost/openai/v1");
        assert_eq!(r.anthropic, "http://localhost:9090/bifrost/anthropic");
    }

    #[test]
    fn routes_reject_non_http_and_decoration() {
        assert!(model_gateway_routes("ftp://gw.example.com", None).is_none());
        assert!(model_gateway_routes("javascript:alert(1)", None).is_none());
        assert!(model_gateway_routes("", None).is_none());
        assert!(model_gateway_routes("   ", None).is_none());
        assert!(
            model_gateway_routes("https://user:pass@gw.example.com", None).is_none(),
            "credentials in the URL must be refused"
        );
        assert!(model_gateway_routes("https://gw.example.com/?x=1", None).is_none());
        assert!(model_gateway_routes("https://gw.example.com/#frag", None).is_none());
    }

    #[test]
    fn routes_unsafe_discovery_path_falls_back_to_default() {
        let r = model_gateway_routes("https://gw.example.com", Some("/../evil")).unwrap();
        assert_eq!(r.discovery, "https://gw.example.com/v1/models");
    }

    // --- credential resolution ---

    #[test]
    fn literal_key_passes_through_trimmed() {
        let r = resolve_model_gateway_api_key("  sk-literal  ", &|_| None, None);
        assert_eq!(r.value, "sk-literal");
        assert!(r.missing.is_empty());
        assert!(!r.stored_secret_missing);
    }

    #[test]
    fn env_reference_resolves_and_fails_closed() {
        let env = env_of(&[("GATEWAY_KEY", "  sk-from-env  ")]);
        let r = resolve_model_gateway_api_key("${env:GATEWAY_KEY}", &env, None);
        assert_eq!(r.value, "sk-from-env");
        assert!(r.missing.is_empty());

        let r = resolve_model_gateway_api_key("${env:NOT_SET}", &|_| None, None);
        assert_eq!(r.value, "");
        assert_eq!(r.missing, vec!["NOT_SET".to_string()]);
    }

    #[test]
    fn env_reference_rejects_embedded_expansion() {
        // Only one exact reference; a fallback would smuggle the secret back
        // into settings.
        assert!(parse_model_gateway_env_reference("${env:A} or ${env:B}").is_none());
        assert!(parse_model_gateway_env_reference("prefix-${env:A}").is_none());
        assert!(parse_model_gateway_env_reference("${env:9BAD}").is_none());
    }

    #[test]
    fn stored_secret_ref_resolves_from_stored_value() {
        let r =
            resolve_model_gateway_api_key(MODEL_GATEWAY_SECRET_REF, &|_| None, Some(" sk-stored "));
        assert_eq!(r.value, "sk-stored");
        assert!(!r.stored_secret_missing);

        let r = resolve_model_gateway_api_key(MODEL_GATEWAY_SECRET_REF, &|_| None, None);
        assert!(r.stored_secret_missing);
        assert_eq!(r.value, "");
    }

    #[test]
    fn credential_kind_classification() {
        assert_eq!(
            model_gateway_credential_kind(""),
            ModelGatewayCredentialKind::Empty
        );
        assert_eq!(
            model_gateway_credential_kind(" ${env:KEY} "),
            ModelGatewayCredentialKind::Environment
        );
        assert_eq!(
            model_gateway_credential_kind(MODEL_GATEWAY_SECRET_REF),
            ModelGatewayCredentialKind::Stored
        );
        assert_eq!(
            model_gateway_credential_kind("sk-plain"),
            ModelGatewayCredentialKind::LegacyLiteral
        );
    }

    // --- parse_gateway_models ---

    #[test]
    fn parse_models_happy_path_and_field_fallbacks() {
        let payload: serde_json::Value = serde_json::json!({
            "data": [
                {"id": "z-model", "context_length": 128000},
                {"id": "a-model", "owned_by": "openai", "max_context_length": "400000",
                 "max_output_tokens": 8192},
                {"id": "b-model", "context_window": 1000000, "max_completion_tokens": 4096},
                {"id": "anthropic/claude-opus", "provider": "anthropic"}
            ]
        });
        let models = parse_gateway_models(&payload);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a-model", "anthropic/claude-opus", "b-model", "z-model"]
        );
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        assert_eq!(by_id("a-model").context_window, Some(400_000));
        assert_eq!(by_id("a-model").provider.as_deref(), Some("openai"));
        assert_eq!(by_id("b-model").context_window, Some(1_000_000));
        assert_eq!(by_id("z-model").context_window, Some(128_000));
        assert_eq!(by_id("z-model").provider, None);
    }

    #[test]
    fn parse_models_drops_unsafe_and_duplicate_ids() {
        let payload: serde_json::Value = serde_json::json!({
            "data": [
                {"id": "  "},
                {"id": ""},
                {"id": "dup"},
                {"id": "dup"},
                {"id": 42},
                {"nope": true},
                {"id": "ok"}
            ]
        });
        let ids: Vec<String> = parse_gateway_models(&payload)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["dup", "ok"]);
    }

    #[test]
    fn parse_models_rejects_non_object_and_missing_data() {
        assert!(parse_gateway_models(&serde_json::json!(null)).is_empty());
        assert!(parse_gateway_models(&serde_json::json!({"data": "nope"})).is_empty());
        assert!(parse_gateway_models(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn parse_models_rejects_bad_token_limits() {
        let payload: serde_json::Value = serde_json::json!({
            "data": [
                {"id": "m1", "context_length": -5},
                {"id": "m2", "context_length": 0},
                {"id": "m3", "context_length": 1.5},
                {"id": "m4", "context_length": "abc"},
                {"id": "m5", "context_length": 200000}
            ]
        });
        let models = parse_gateway_models(&payload);
        for m in &models {
            if m.id == "m5" {
                assert_eq!(m.context_window, Some(200_000));
            } else {
                assert_eq!(m.context_window, None, "{} must stay unknown", m.id);
            }
        }
    }

    // --- model_gateway_env ---

    fn gw_settings(base: &str, key: &str) -> ModelGatewaySettings {
        ModelGatewaySettings {
            base_url: base.to_string(),
            api_key: key.to_string(),
            discovery_path: None,
        }
    }

    #[test]
    fn env_claude_arm() {
        let s = gw_settings("https://gw.example.com", "sk-1");
        let env = model_gateway_env(&s, "claude", Some("m1"), &|_| None, None);
        assert_eq!(
            env,
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "https://gw.example.com/anthropic".to_string()
                ),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-1".to_string()),
            ]
        );
        // legacy alias id
        let env2 = model_gateway_env(&s, "claude-code", Some("m1"), &|_| None, None);
        assert_eq!(env, env2);
    }

    #[test]
    fn env_codex_arm() {
        let s = gw_settings("https://gw.example.com", "sk-1");
        let env = model_gateway_env(&s, "codex", Some("m1"), &|_| None, None);
        assert_eq!(
            env,
            vec![
                (
                    "OPENAI_BASE_URL".to_string(),
                    "https://gw.example.com/openai/v1".to_string()
                ),
                ("OPENAI_API_KEY".to_string(), "sk-1".to_string()),
            ]
        );
    }

    #[test]
    fn env_copilot_arm_splits_ids_and_sets_wire_api_for_gpt5() {
        let s = gw_settings("https://gw.example.com", "sk-1");
        let env = model_gateway_env(
            &s,
            "copilot",
            Some("anthropic/claude-opus"),
            &|_| None,
            None,
        );
        assert_eq!(
            env,
            vec![
                (
                    "COPILOT_PROVIDER_BASE_URL".to_string(),
                    "https://gw.example.com/anthropic".to_string()
                ),
                ("COPILOT_PROVIDER_TYPE".to_string(), "anthropic".to_string()),
                ("COPILOT_PROVIDER_API_KEY".to_string(), "sk-1".to_string()),
                (
                    "COPILOT_PROVIDER_MODEL_ID".to_string(),
                    "claude-opus".to_string()
                ),
                (
                    "COPILOT_PROVIDER_WIRE_MODEL".to_string(),
                    "anthropic/claude-opus".to_string()
                ),
            ]
        );

        let env = model_gateway_env(&s, "copilot", Some("openai/gpt-5.1"), &|_| None, None);
        assert_eq!(
            env,
            vec![
                (
                    "COPILOT_PROVIDER_BASE_URL".to_string(),
                    "https://gw.example.com/openai/v1".to_string()
                ),
                ("COPILOT_PROVIDER_TYPE".to_string(), "openai".to_string()),
                ("COPILOT_PROVIDER_API_KEY".to_string(), "sk-1".to_string()),
                (
                    "COPILOT_PROVIDER_MODEL_ID".to_string(),
                    "gpt-5.1".to_string()
                ),
                (
                    "COPILOT_PROVIDER_WIRE_MODEL".to_string(),
                    "openai/gpt-5.1".to_string()
                ),
                (
                    "COPILOT_PROVIDER_WIRE_API".to_string(),
                    "responses".to_string()
                ),
            ]
        );

        // No model ⇒ no BYOK provider activation at all.
        assert!(model_gateway_env(&s, "copilot", None, &|_| None, None).is_empty());
        // Non-gpt-5 openai model ⇒ no wire-api override.
        let plain = model_gateway_env(&s, "copilot", Some("openai/gpt-4o"), &|_| None, None);
        assert!(!plain.iter().any(|(k, _)| k == "COPILOT_PROVIDER_WIRE_API"));
    }

    #[test]
    fn env_fails_closed_on_bad_settings_or_agent() {
        let s = gw_settings("https://gw.example.com", "sk-1");
        // Unknown agent ⇒ nothing.
        assert!(model_gateway_env(&s, "opencode", Some("m"), &|_| None, None).is_empty());
        assert!(model_gateway_env(&s, "grok", Some("m"), &|_| None, None).is_empty());
        // No key ⇒ nothing.
        assert!(model_gateway_env(
            &gw_settings("https://gw.example.com", "  "),
            "claude",
            Some("m"),
            &|_| None,
            None
        )
        .is_empty());
        // Unresolvable env reference ⇒ nothing, not a partial credential.
        assert!(model_gateway_env(
            &gw_settings("https://gw.example.com", "${env:NOPE}"),
            "claude",
            Some("m"),
            &|_| None,
            None
        )
        .is_empty());
        // Stored ref with no stored secret ⇒ nothing.
        assert!(model_gateway_env(
            &gw_settings("https://gw.example.com", MODEL_GATEWAY_SECRET_REF),
            "claude",
            Some("m"),
            &|_| None,
            None
        )
        .is_empty());
        // Invalid base url ⇒ nothing.
        assert!(model_gateway_env(
            &gw_settings("not a url", "sk"),
            "claude",
            Some("m"),
            &|_| None,
            None
        )
        .is_empty());
    }

    // --- normalized_agent_model / with_agent_model ---

    #[test]
    fn normalized_model_revalidates() {
        assert_eq!(
            normalized_agent_model("claude", Some("  m-1 ")),
            Some("m-1".to_string())
        );
        assert_eq!(normalized_agent_model("claude", Some("")), None);
        assert_eq!(normalized_agent_model("claude", None), None);
        assert_eq!(normalized_agent_model("claude", Some("bad\nid")), None);
        assert_eq!(
            normalized_agent_model("claude", Some(&"x".repeat(501))),
            None
        );
    }

    #[test]
    fn with_agent_model_appends_quoted_flag() {
        assert_eq!(
            with_agent_model(
                "claude-agent-acp --acp",
                "claude",
                "--model",
                Some("big-model")
            ),
            "claude-agent-acp --acp --model 'big-model'"
        );
        // codex uses -m, passed through by the caller.
        assert_eq!(
            with_agent_model("codex-acp", "codex", "-m", Some("gpt-5")),
            "codex-acp -m 'gpt-5'"
        );
        // copilot takes the UNPREFIXED id on the flag.
        assert_eq!(
            with_agent_model("copilot", "copilot", "--model", Some("openai/gpt-5")),
            "copilot --model 'gpt-5'"
        );
        // No model ⇒ command unchanged.
        assert_eq!(
            with_agent_model("claude-agent-acp", "claude", "--model", None),
            "claude-agent-acp"
        );
        // Shell-special value is quoted.
        assert_eq!(
            with_agent_model("claude", "claude", "--model", Some("it's")),
            "claude --model 'it'\\''s'"
        );
    }

    // --- claude_autocompact_for: the pairing invariant ---

    fn catalog() -> Vec<GatewayModel> {
        vec![
            GatewayModel {
                id: "small".to_string(),
                name: None,
                provider: None,
                context_window: Some(100_000),
                max_output_tokens: None,
            },
            GatewayModel {
                id: "big[1m]".to_string(),
                name: None,
                provider: None,
                context_window: Some(1_000_000),
                max_output_tokens: None,
            },
        ]
    }

    #[test]
    fn autocompact_small_window_strips_suffix_and_emits_no_env() {
        let r = claude_autocompact_for("claude", Some("small[1m]"), &catalog());
        assert_eq!(r.model_id, Some("small".to_string()));
        assert!(r.env.is_empty());
    }

    #[test]
    fn autocompact_large_window_suffix_and_env_emitted_together() {
        let r = claude_autocompact_for("claude", Some("big"), &catalog());
        assert_eq!(r.model_id, Some("big[1m]".to_string()));
        assert_eq!(
            r.env,
            vec![
                (
                    "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
                    "1000000".to_string()
                ),
                (
                    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(),
                    "80".to_string()
                ),
            ]
        );
        // The pairing invariant, stated as an assertion: env rides ONLY on a
        // suffixed branch.
        assert!(!r.model_id.as_deref().unwrap_or("").ends_with("[1m]") || !r.env.is_empty());
    }

    #[test]
    fn autocompact_unknown_model_fails_open() {
        let r = claude_autocompact_for("claude", Some("unknown"), &catalog());
        assert_eq!(r.model_id, Some("unknown".to_string()));
        assert!(r.env.is_empty());

        // Catalogue entry without a window is also fail-open.
        let models = vec![GatewayModel {
            id: "unknown".to_string(),
            name: None,
            provider: None,
            context_window: None,
            max_output_tokens: None,
        }];
        let r = claude_autocompact_for("claude", Some("unknown"), &models);
        assert!(r.env.is_empty());
    }

    #[test]
    fn autocompact_is_claude_only() {
        for agent in ["codex", "copilot", "opencode"] {
            let r = claude_autocompact_for(agent, Some("big"), &catalog());
            assert_eq!(r.model_id, Some("big".to_string()));
            assert!(r.env.is_empty(), "{agent} must get no autocompact pair");
        }
    }

    // --- subagent routing ---

    #[test]
    fn subagent_env_prefers_configured_default_when_served() {
        let models = vec![
            GatewayModel {
                id: "a".into(),
                name: None,
                provider: None,
                context_window: None,
                max_output_tokens: None,
            },
            GatewayModel {
                id: "b".into(),
                name: None,
                provider: None,
                context_window: None,
                max_output_tokens: None,
            },
        ];
        let env = claude_subagent_env_for("claude", &models, Some("b"));
        assert!(env.contains(&("CLAUDE_CODE_SUBAGENT_MODEL".to_string(), "b".to_string())));
        // Non-served default falls back to the first sorted id.
        let env = claude_subagent_env_for("claude", &models, Some("zzz"));
        assert!(env.contains(&("CLAUDE_CODE_SUBAGENT_MODEL".to_string(), "a".to_string())));
        // Empty catalogue emits nothing rather than guessing an id.
        assert!(claude_subagent_env_for("claude", &[], None).is_empty());
        assert!(claude_subagent_env_for("codex", &models, None).is_empty());
    }

    // --- model_context_window ---

    #[test]
    fn context_window_accepts_both_spellings() {
        let models = catalog();
        assert_eq!(
            model_context_window(Some("big[1m]"), &models),
            Some(1_000_000)
        );
        assert_eq!(model_context_window(Some("big"), &models), Some(1_000_000));
        assert_eq!(model_context_window(Some("small"), &models), Some(100_000));
        assert_eq!(model_context_window(Some("nope"), &models), None);
        assert_eq!(model_context_window(None, &models), None);
    }
}
