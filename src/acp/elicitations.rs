//! Form-mode ACP elicitation, normalized for the structured view.
//!
//! claude-agent-acp (>=0.44) re-enables the built-in `AskUserQuestion`
//! tool only when the client advertises `elicitation.form`, then routes
//! the question(s) to us as an `elicitation/create` request carrying a
//! JSON-Schema form. The same `elicitation.form` capability also lets an
//! MCP server attached to the agent collect arbitrary structured input,
//! which arrives through the identical path with a richer schema (number,
//! integer, boolean fields; length / range / pattern / format
//! constraints; defaults). This module owns the boundary between that raw
//! ACP schema and a clean, web-facing view model:
//!
//! - [`parse_elicitation`] turns a [`CreateElicitationRequest`] into a
//!   normalized [`Elicitation`] (a list of questions with options),
//!   classifying each form field by its JSON-Schema shape rather than by
//!   the adapter's specific field keys, so the structured view never has
//!   to understand `oneOf`/`anyOf`/`enum`.
//! - [`build_response`] validates the user's selection against that
//!   normalized model (never trusting the browser to send valid option
//!   values back into a tool result) and builds the
//!   [`CreateElicitationResponse`] the agent expects.
//!
//! The server generates a single-use [`Nonce`] for each elicitation,
//! mirroring the approval flow: it travels client -> server only on
//! resolution, so a malicious agent can neither synthesize nor replay a
//! resolution.

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
    ElicitationSchema, ElicitationScope, MultiSelectItems, StringFormat, StringPropertySchema,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::approvals::Nonce;

/// A pending or resolved elicitation. Held in
/// `AcpState::pending_elicitations` until it is resolved through
/// `apply_event(Event::ElicitationResolved { ... })`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Elicitation {
    pub nonce: Nonce,
    /// Human-readable prompt. For a single AskUserQuestion this is the
    /// question text; for multiple it is a short lead-in.
    pub message: String,
    /// Optional schema-level title (MCP elicitations may set one;
    /// AskUserQuestion does not). Rendered as the form heading.
    pub title: Option<String>,
    /// Optional schema-level description, rendered under the message.
    pub description: Option<String>,
    /// Tool call this elicitation belongs to, when the agent scoped it to
    /// one. Lets the UI render the card under the originating tool.
    pub tool_call_id: Option<String>,
    pub questions: Vec<ElicitationQuestion>,
    pub requested_at: DateTime<Utc>,
    pub resolved: Option<ResolvedElicitation>,
}

/// One field of the elicitation form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationQuestion {
    /// Schema property key (`question_0`, `question_0_custom`, ...). Echoed
    /// back verbatim as the answer key in the response content.
    pub field_key: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub kind: ElicitationFieldKind,
    /// Selectable options for `SingleSelect` / `MultiSelect`; empty for
    /// every other kind.
    pub options: Vec<ElicitationOption>,
    /// Multi-select bounds.
    pub min_items: Option<u64>,
    pub max_items: Option<u64>,
    /// String bounds (`FreeText`).
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    /// Regular expression the string must match (`FreeText`).
    pub pattern: Option<String>,
    /// String format annotation (`email`, `uri`, `date`, `date-time`, or
    /// a passthrough custom token); a UI hint only, never a hard gate.
    pub format: Option<String>,
    /// Numeric bounds (`Number` / `Integer`), kept as `f64` so a single
    /// pair covers both; integer fields still validate integrality.
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    /// Pre-fill value, shaped to match the field's answer kind.
    pub default: Option<AnswerValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationFieldKind {
    /// Plain string input (the AskUserQuestion "custom answer" box, or any
    /// unconstrained MCP string field).
    FreeText,
    /// Pick exactly one option (rendered as radios).
    SingleSelect,
    /// Pick zero or more options (rendered as checkboxes).
    MultiSelect,
    /// Floating-point number input.
    Number,
    /// Integer number input.
    Integer,
    /// Boolean input (rendered as a checkbox / toggle).
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitationOption {
    /// Value echoed back to the agent. For AskUserQuestion the adapter
    /// uses the option label as the value.
    pub value: String,
    /// Human-readable label.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedElicitation {
    pub outcome: ElicitationOutcome,
    pub resolved_at: DateTime<Utc>,
}

/// One answered question, rendered for the transcript. `question` is the
/// human-readable prompt (the question title, or the field key as a
/// fallback); `answer` is the display value the user submitted. Computed
/// server-side at resolve time and carried on `Event::ElicitationResolved`
/// so the structured view can show what the user picked after the card
/// closes. See #2209.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationAnswer {
    pub question: String,
    pub answer: String,
}

/// Separator the claude-agent-acp adapter wedges between an AskUserQuestion
/// option's label and its description when flattening into the enum title
/// (`"<label> <sep> <description>"`). Written as an escape so the em dash
/// never appears literally in source. Mirrors the web card's separator.
const OPTION_DESC_SEP: &str = " \u{2014} ";

/// Render the user's submitted answers into display-ready pairs, in the
/// form's question order. Only answered questions are included (an optional
/// question left blank is omitted). Select values are mapped to their option
/// label; for AskUserQuestion the value is already the label (and `label` may
/// carry a trailing description, which is dropped), while a generic MCP form
/// maps its machine token to the human label.
pub fn summarize_answers(
    elicitation: &Elicitation,
    answers: &BTreeMap<String, AnswerValue>,
) -> Vec<ElicitationAnswer> {
    let mut out = Vec::new();
    for question in &elicitation.questions {
        let Some(value) = answers.get(&question.field_key) else {
            continue;
        };
        // Map a selected option value to its human label. For a generic MCP
        // form the value is a machine token and the label the display text; for
        // AskUserQuestion the value is already the label (and `label` may carry
        // a `"value <sep> description"` form, so we keep the bare value there).
        // Mirrors `optionParts` in the web AskUserQuestionCard. See #2209.
        let label_for = |raw: &str| -> String {
            match question.options.iter().find(|o| o.value == raw) {
                Some(o) if !o.label.starts_with(&format!("{raw}{OPTION_DESC_SEP}")) => {
                    o.label.clone()
                }
                _ => raw.to_string(),
            }
        };
        let answer = match value {
            AnswerValue::Bool(b) => {
                if *b {
                    "Yes".to_string()
                } else {
                    "No".to_string()
                }
            }
            AnswerValue::Integer(i) => i.to_string(),
            AnswerValue::Number(n) => n.to_string(),
            AnswerValue::Text(s) => label_for(s),
            AnswerValue::List(values) => values
                .iter()
                .map(|v| label_for(v))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let question = question
            .title
            .clone()
            .unwrap_or_else(|| question.field_key.clone());
        out.push(ElicitationAnswer { question, answer });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElicitationOutcome {
    /// User submitted answers (ACP `accept`).
    Accepted,
    /// User skipped (ACP `decline`): the agent continues with no answer.
    Declined,
    /// Cancelled (ACP `cancel`), or torn down without a user decision
    /// (daemon restart, agent cancel). The agent's tool call aborts.
    Cancelled,
}

/// Reason a form schema could not be normalized for the structured view.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ElicitationParseError {
    /// URL-mode elicitation. The structured view only renders forms; we
    /// do not advertise `elicitation.url`, so this should not occur, but
    /// reject loudly rather than rendering nothing.
    #[error("elicitation is not form-mode")]
    NotFormMode,
    /// A field used a JSON-Schema kind the structured view cannot render.
    /// All of the kinds the ACP schema currently defines are handled; this
    /// guards against a future `#[non_exhaustive]` property variant.
    #[error("elicitation field {0:?} uses an unsupported schema kind")]
    UnsupportedField(String),
}

/// Why a submitted answer set was rejected before reaching the agent.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ElicitationValidationError {
    #[error("answer for unknown field {0:?}")]
    UnknownField(String),
    #[error("field {0:?} expected a {1} value")]
    WrongValueType(String, &'static str),
    #[error("field {field:?} got option {value:?} which is not offered")]
    InvalidOption { field: String, value: String },
    #[error("required field {0:?} was not answered")]
    MissingRequired(String),
    #[error("field {field:?} needs at least {min} selection(s)")]
    TooFewItems { field: String, min: u64 },
    #[error("field {field:?} allows at most {max} selection(s)")]
    TooManyItems { field: String, max: u64 },
    #[error("field {field:?} must be at least {min} character(s)")]
    TooShort { field: String, min: u32 },
    #[error("field {field:?} must be at most {max} character(s)")]
    TooLong { field: String, max: u32 },
    #[error("field {field:?} does not match the required pattern")]
    PatternMismatch { field: String },
    #[error("field {field:?} is out of the allowed range")]
    OutOfRange { field: String },
    #[error("field {field:?} must be a whole number")]
    NotAnInteger { field: String },
}

/// The user's decision, as sent by the web client on resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ElicitationResolution {
    /// User submitted the form. `answers` maps each answered field key to
    /// its value. Unanswered optional fields may be omitted.
    Accept {
        #[serde(default)]
        answers: BTreeMap<String, AnswerValue>,
    },
    /// User skipped the form (ACP `decline`).
    Decline,
    /// User aborted the agent's tool call (ACP `cancel`).
    Cancel,
}

/// A submitted (or default) answer value. Untagged: the variant is chosen
/// by JSON shape, so the order matters. `Bool` and the integer case are
/// tried before `Number`/`Text` so `true` and `5` do not deserialize as a
/// float or a string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnswerValue {
    Bool(bool),
    Integer(i64),
    Number(f64),
    Text(String),
    List(Vec<String>),
}

impl ElicitationResolution {
    pub fn outcome(&self) -> ElicitationOutcome {
        match self {
            ElicitationResolution::Accept { .. } => ElicitationOutcome::Accepted,
            ElicitationResolution::Decline => ElicitationOutcome::Declined,
            ElicitationResolution::Cancel => ElicitationOutcome::Cancelled,
        }
    }
}

/// Order a form's properties for display. The adapter keys questions
/// `question_0..N`, each optionally followed by its own free-text "Other"
/// box `question_<n>_custom`, and serializes them through a `BTreeMap`,
/// which sorts lexically (`question_10` before `question_2`, and every
/// `question_<n>_custom` after every `question_<n>`). Recover the numeric
/// order and keep each `_custom` box adjacent to its question, so the
/// per-question "Other" field renders next to the question it belongs to
/// rather than bunched at the end. Non-`question_N` keys (arbitrary MCP
/// form fields) sort after, by key.
fn ordered_fields(
    properties: &BTreeMap<String, ElicitationPropertySchema>,
) -> Vec<(&String, &ElicitationPropertySchema)> {
    // `(index, is_custom)`: the base question (0) sorts before its paired
    // `_custom` box (1), and both before the next question's index.
    fn question_order(key: &str) -> Option<(u64, u8)> {
        let rest = key.strip_prefix("question_")?;
        match rest.strip_suffix("_custom") {
            Some(index) => Some((index.parse().ok()?, 1)),
            None => Some((rest.parse().ok()?, 0)),
        }
    }
    let mut fields: Vec<_> = properties.iter().collect();
    fields.sort_by(
        |(a, _), (b, _)| match (question_order(a), question_order(b)) {
            (Some(ai), Some(bi)) => ai.cmp(&bi),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.cmp(b),
        },
    );
    fields
}

/// Render a `StringFormat` as the wire token (`email`, `uri`, `date`,
/// `date-time`) so the web can map it to an input type. Unknown formats
/// pass through verbatim; the spec treats them as advisory annotations.
fn format_token(format: &StringFormat) -> String {
    match format {
        StringFormat::Email => "email".to_string(),
        StringFormat::Uri => "uri".to_string(),
        StringFormat::Date => "date".to_string(),
        StringFormat::DateTime => "date-time".to_string(),
        // `StringFormat` is non_exhaustive; a future token surfaces as a
        // generic annotation rather than failing the parse.
        _ => "unknown".to_string(),
    }
}

fn empty_question(
    field_key: &str,
    kind: ElicitationFieldKind,
    required: bool,
) -> ElicitationQuestion {
    ElicitationQuestion {
        field_key: field_key.to_string(),
        title: None,
        description: None,
        required,
        kind,
        options: Vec::new(),
        min_items: None,
        max_items: None,
        min_length: None,
        max_length: None,
        pattern: None,
        format: None,
        minimum: None,
        maximum: None,
        default: None,
    }
}

fn parse_string_field(
    field_key: &str,
    s: &StringPropertySchema,
    required: bool,
) -> ElicitationQuestion {
    // `oneOf` carries titled options; `enum` carries bare values used as
    // both value and label; neither means a free-text field.
    let (kind, options) = if let Some(one_of) = &s.one_of {
        (
            ElicitationFieldKind::SingleSelect,
            one_of
                .iter()
                .map(|o| ElicitationOption {
                    value: o.value.clone(),
                    label: o.title.clone(),
                })
                .collect(),
        )
    } else if let Some(enum_values) = &s.enum_values {
        (
            ElicitationFieldKind::SingleSelect,
            enum_values
                .iter()
                .map(|v| ElicitationOption {
                    value: v.clone(),
                    label: v.clone(),
                })
                .collect(),
        )
    } else {
        (ElicitationFieldKind::FreeText, Vec::new())
    };
    ElicitationQuestion {
        title: s.title.clone(),
        description: s.description.clone(),
        kind,
        options,
        min_length: s.min_length,
        max_length: s.max_length,
        pattern: s.pattern.clone(),
        format: s.format.as_ref().map(format_token),
        default: s.default.clone().map(AnswerValue::Text),
        ..empty_question(field_key, ElicitationFieldKind::FreeText, required)
    }
}

fn parse_field(
    field_key: &str,
    prop: &ElicitationPropertySchema,
    required: bool,
) -> Result<ElicitationQuestion, ElicitationParseError> {
    match prop {
        ElicitationPropertySchema::String(s) => Ok(parse_string_field(field_key, s, required)),
        ElicitationPropertySchema::Array(a) => {
            let options = match &a.items {
                MultiSelectItems::Titled(t) => t
                    .options
                    .iter()
                    .map(|o| ElicitationOption {
                        value: o.value.clone(),
                        label: o.title.clone(),
                    })
                    .collect(),
                MultiSelectItems::Untitled(u) => u
                    .values
                    .iter()
                    .map(|v| ElicitationOption {
                        value: v.clone(),
                        label: v.clone(),
                    })
                    .collect(),
                // `MultiSelectItems` is non_exhaustive; a future item shape
                // surfaces as an option-less multi-select rather than a hard
                // failure.
                _ => Vec::new(),
            };
            Ok(ElicitationQuestion {
                title: a.title.clone(),
                description: a.description.clone(),
                options,
                min_items: a.min_items,
                max_items: a.max_items,
                default: a.default.clone().map(AnswerValue::List),
                ..empty_question(field_key, ElicitationFieldKind::MultiSelect, required)
            })
        }
        ElicitationPropertySchema::Number(n) => Ok(ElicitationQuestion {
            title: n.title.clone(),
            description: n.description.clone(),
            minimum: n.minimum,
            maximum: n.maximum,
            default: n.default.map(AnswerValue::Number),
            ..empty_question(field_key, ElicitationFieldKind::Number, required)
        }),
        ElicitationPropertySchema::Integer(i) => Ok(ElicitationQuestion {
            title: i.title.clone(),
            description: i.description.clone(),
            minimum: i.minimum.map(|v| v as f64),
            maximum: i.maximum.map(|v| v as f64),
            default: i.default.map(AnswerValue::Integer),
            ..empty_question(field_key, ElicitationFieldKind::Integer, required)
        }),
        ElicitationPropertySchema::Boolean(b) => Ok(ElicitationQuestion {
            title: b.title.clone(),
            description: b.description.clone(),
            default: b.default.map(AnswerValue::Bool),
            ..empty_question(field_key, ElicitationFieldKind::Boolean, required)
        }),
        // Guards a future `#[non_exhaustive]` property variant the schema
        // crate may add; every kind defined today is handled above.
        _ => Err(ElicitationParseError::UnsupportedField(
            field_key.to_string(),
        )),
    }
}

/// Normalize a form-mode `elicitation/create` request into the view model
/// the structured view renders.
pub fn parse_elicitation(
    nonce: Nonce,
    request: &CreateElicitationRequest,
    requested_at: DateTime<Utc>,
) -> Result<Elicitation, ElicitationParseError> {
    let ElicitationMode::Form(form) = &request.mode else {
        return Err(ElicitationParseError::NotFormMode);
    };
    let tool_call_id = match &form.scope {
        ElicitationScope::Session(scope) => scope.tool_call_id.as_ref().map(|id| id.0.to_string()),
        // Request-scoped (pre-session) elicitations, plus any future
        // scope variant: no tool call to anchor the card to.
        _ => None,
    };
    let schema: &ElicitationSchema = &form.requested_schema;
    let required = schema.required.clone().unwrap_or_default();
    let mut questions = Vec::with_capacity(schema.properties.len());
    for (field_key, prop) in ordered_fields(&schema.properties) {
        questions.push(parse_field(field_key, prop, required.contains(field_key))?);
    }
    Ok(Elicitation {
        nonce,
        message: request.message.clone(),
        title: schema.title.clone(),
        description: schema.description.clone(),
        tool_call_id,
        questions,
        requested_at,
        resolved: None,
    })
}

/// Validate the text-shaped value of a free-text or single-select field
/// and return the string to store, or `None` when the (optional) field was
/// left blank.
fn validate_text(
    question: &ElicitationQuestion,
    answer: Option<&AnswerValue>,
) -> Result<Option<String>, ElicitationValidationError> {
    let text = match answer {
        Some(AnswerValue::Text(text)) => text.clone(),
        // Numbers / booleans coerce to their textual form so a free-text
        // field stays forgiving; selects are checked against options below.
        Some(AnswerValue::Integer(i)) => i.to_string(),
        Some(AnswerValue::Number(n)) => n.to_string(),
        Some(AnswerValue::Bool(b)) => b.to_string(),
        Some(AnswerValue::List(_)) => {
            return Err(ElicitationValidationError::WrongValueType(
                question.field_key.clone(),
                "string",
            ));
        }
        None => String::new(),
    };

    if matches!(question.kind, ElicitationFieldKind::SingleSelect)
        && !text.is_empty()
        && !question.options.iter().any(|o| o.value == text)
    {
        return Err(ElicitationValidationError::InvalidOption {
            field: question.field_key.clone(),
            value: text,
        });
    }

    if text.is_empty() {
        if question.required {
            return Err(ElicitationValidationError::MissingRequired(
                question.field_key.clone(),
            ));
        }
        return Ok(None);
    }

    // Length / pattern constraints only apply to a value the user actually
    // typed; selects answer from a fixed option set, so skip them there.
    if matches!(question.kind, ElicitationFieldKind::FreeText) {
        let len = text.chars().count() as u32;
        if let Some(min) = question.min_length {
            if len < min {
                return Err(ElicitationValidationError::TooShort {
                    field: question.field_key.clone(),
                    min,
                });
            }
        }
        if let Some(max) = question.max_length {
            if len > max {
                return Err(ElicitationValidationError::TooLong {
                    field: question.field_key.clone(),
                    max,
                });
            }
        }
        if let Some(pattern) = &question.pattern {
            // An invalid pattern from the agent is treated as no constraint
            // rather than rejecting every answer.
            if let Ok(re) = regex::Regex::new(pattern) {
                if !re.is_match(&text) {
                    return Err(ElicitationValidationError::PatternMismatch {
                        field: question.field_key.clone(),
                    });
                }
            }
        }
    }

    Ok(Some(text))
}

fn check_range(
    question: &ElicitationQuestion,
    value: f64,
) -> Result<(), ElicitationValidationError> {
    if question.minimum.is_some_and(|min| value < min)
        || question.maximum.is_some_and(|max| value > max)
    {
        return Err(ElicitationValidationError::OutOfRange {
            field: question.field_key.clone(),
        });
    }
    Ok(())
}

/// Validate a user resolution against the normalized form and build the
/// ACP response. Accept answers are checked server-side: every key must be
/// a known field, value shapes must match the field kind, selected values
/// must be offered options, and required / length / range / pattern / item
/// constraints must hold. This is the only place answers cross back to the
/// agent, so the browser is never trusted to send well-formed content.
pub fn build_response(
    elicitation: &Elicitation,
    resolution: ElicitationResolution,
) -> Result<CreateElicitationResponse, ElicitationValidationError> {
    let answers = match resolution {
        ElicitationResolution::Decline => {
            return Ok(CreateElicitationResponse::new(ElicitationAction::Decline));
        }
        ElicitationResolution::Cancel => {
            return Ok(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
        ElicitationResolution::Accept { answers } => answers,
    };

    // Reject answers for fields the form never offered.
    for key in answers.keys() {
        if !elicitation.questions.iter().any(|q| &q.field_key == key) {
            return Err(ElicitationValidationError::UnknownField(key.clone()));
        }
    }

    let mut content: BTreeMap<String, ElicitationContentValue> = BTreeMap::new();
    for question in &elicitation.questions {
        let answer = answers.get(&question.field_key);
        match question.kind {
            ElicitationFieldKind::MultiSelect => {
                let selected = match answer {
                    Some(AnswerValue::List(values)) => values.clone(),
                    Some(_) => {
                        return Err(ElicitationValidationError::WrongValueType(
                            question.field_key.clone(),
                            "list",
                        ));
                    }
                    None => Vec::new(),
                };
                for value in &selected {
                    if !question.options.iter().any(|o| &o.value == value) {
                        return Err(ElicitationValidationError::InvalidOption {
                            field: question.field_key.clone(),
                            value: value.clone(),
                        });
                    }
                }
                // An unanswered question is "required?" only: min_items /
                // max_items constrain a selection the user actually made, so
                // an optional field with min_items > 0 must not error when
                // left blank.
                if selected.is_empty() {
                    if question.required {
                        return Err(ElicitationValidationError::MissingRequired(
                            question.field_key.clone(),
                        ));
                    }
                    continue;
                }
                if let Some(min) = question.min_items {
                    if (selected.len() as u64) < min {
                        return Err(ElicitationValidationError::TooFewItems {
                            field: question.field_key.clone(),
                            min,
                        });
                    }
                }
                if let Some(max) = question.max_items {
                    if (selected.len() as u64) > max {
                        return Err(ElicitationValidationError::TooManyItems {
                            field: question.field_key.clone(),
                            max,
                        });
                    }
                }
                content.insert(
                    question.field_key.clone(),
                    ElicitationContentValue::StringArray(selected),
                );
            }
            ElicitationFieldKind::SingleSelect | ElicitationFieldKind::FreeText => {
                if let Some(text) = validate_text(question, answer)? {
                    content.insert(
                        question.field_key.clone(),
                        ElicitationContentValue::String(text),
                    );
                }
            }
            ElicitationFieldKind::Number => {
                let value = match answer {
                    Some(AnswerValue::Number(n)) => *n,
                    Some(AnswerValue::Integer(i)) => *i as f64,
                    Some(_) => {
                        return Err(ElicitationValidationError::WrongValueType(
                            question.field_key.clone(),
                            "number",
                        ));
                    }
                    None => {
                        if question.required {
                            return Err(ElicitationValidationError::MissingRequired(
                                question.field_key.clone(),
                            ));
                        }
                        continue;
                    }
                };
                check_range(question, value)?;
                content.insert(
                    question.field_key.clone(),
                    ElicitationContentValue::Number(value),
                );
            }
            ElicitationFieldKind::Integer => {
                let value = match answer {
                    Some(AnswerValue::Integer(i)) => *i,
                    // A whole-valued float in range (the browser may send
                    // `5` as a JSON number) coerces. `as i64` saturates, so
                    // an out-of-range or non-finite float must be rejected
                    // before the cast or it would silently clamp to
                    // i64::MIN / i64::MAX and slip past check_range.
                    Some(AnswerValue::Number(n))
                        if n.is_finite()
                            && n.fract() == 0.0
                            && *n >= i64::MIN as f64
                            && *n <= i64::MAX as f64 =>
                    {
                        *n as i64
                    }
                    Some(AnswerValue::Number(n)) if n.is_finite() && n.fract() != 0.0 => {
                        return Err(ElicitationValidationError::NotAnInteger {
                            field: question.field_key.clone(),
                        });
                    }
                    // Non-finite or out-of-i64-range whole float.
                    Some(AnswerValue::Number(_)) => {
                        return Err(ElicitationValidationError::OutOfRange {
                            field: question.field_key.clone(),
                        });
                    }
                    Some(_) => {
                        return Err(ElicitationValidationError::WrongValueType(
                            question.field_key.clone(),
                            "integer",
                        ));
                    }
                    None => {
                        if question.required {
                            return Err(ElicitationValidationError::MissingRequired(
                                question.field_key.clone(),
                            ));
                        }
                        continue;
                    }
                };
                check_range(question, value as f64)?;
                content.insert(
                    question.field_key.clone(),
                    ElicitationContentValue::Integer(value),
                );
            }
            ElicitationFieldKind::Boolean => {
                let value = match answer {
                    Some(AnswerValue::Bool(b)) => *b,
                    Some(_) => {
                        return Err(ElicitationValidationError::WrongValueType(
                            question.field_key.clone(),
                            "boolean",
                        ));
                    }
                    None => {
                        if question.required {
                            return Err(ElicitationValidationError::MissingRequired(
                                question.field_key.clone(),
                            ));
                        }
                        continue;
                    }
                };
                content.insert(
                    question.field_key.clone(),
                    ElicitationContentValue::Boolean(value),
                );
            }
        }
    }

    Ok(CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(content),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        BooleanPropertySchema, ElicitationFormMode, ElicitationSessionScope, EnumOption,
        IntegerPropertySchema, MultiSelectPropertySchema, NumberPropertySchema,
        StringPropertySchema,
    };

    fn form_request(schema: ElicitationSchema, message: &str) -> CreateElicitationRequest {
        CreateElicitationRequest::new(
            ElicitationFormMode::new(ElicitationSessionScope::new("sess-1"), schema),
            message,
        )
    }

    fn single_question_schema() -> ElicitationSchema {
        ElicitationSchema::new().property(
            "question_0",
            StringPropertySchema::new().title("Pick one").one_of(vec![
                EnumOption::new("Yes", "Yes"),
                EnumOption::new("No", "No"),
            ]),
            true,
        )
    }

    #[test]
    fn parses_single_select_one_of() {
        let req = form_request(single_question_schema(), "Pick one?");
        let e = parse_elicitation(Nonce::new(), &req, Utc::now()).unwrap();
        assert_eq!(e.message, "Pick one?");
        assert_eq!(e.questions.len(), 1);
        let q = &e.questions[0];
        assert_eq!(q.kind, ElicitationFieldKind::SingleSelect);
        assert!(q.required);
        assert_eq!(q.options.len(), 2);
        assert_eq!(q.options[0].value, "Yes");
    }

    #[test]
    fn parses_multi_select_and_free_text_and_orders_numerically() {
        let schema = ElicitationSchema::new()
            .property(
                "question_10",
                MultiSelectPropertySchema::titled(vec![
                    EnumOption::new("a", "Apple"),
                    EnumOption::new("b", "Banana"),
                ]),
                false,
            )
            .property("question_2", StringPropertySchema::new(), false)
            .property(
                "customAnswer",
                StringPropertySchema::new().title("Other"),
                false,
            );
        let req = form_request(schema, "many");
        let e = parse_elicitation(Nonce::new(), &req, Utc::now()).unwrap();
        // question_2 before question_10 (numeric), customAnswer last.
        assert_eq!(e.questions[0].field_key, "question_2");
        assert_eq!(e.questions[0].kind, ElicitationFieldKind::FreeText);
        assert_eq!(e.questions[1].field_key, "question_10");
        assert_eq!(e.questions[1].kind, ElicitationFieldKind::MultiSelect);
        assert_eq!(e.questions[2].field_key, "customAnswer");
    }

    #[test]
    fn per_question_custom_box_stays_next_to_its_question() {
        // claude-agent-acp >=0.46 emits a per-question "Other" box keyed
        // `question_<n>_custom` right after each `question_<n>`. The BTreeMap
        // would otherwise sort every `_custom` after every question; the
        // ordering must instead keep each box adjacent to its question.
        let schema = ElicitationSchema::new()
            .string("question_0", false)
            .property(
                "question_0_custom",
                StringPropertySchema::new().title("Other"),
                false,
            )
            .string("question_1", false)
            .property(
                "question_1_custom",
                StringPropertySchema::new().title("Other"),
                false,
            );
        let req = form_request(schema, "many");
        let e = parse_elicitation(Nonce::new(), &req, Utc::now()).unwrap();
        let keys: Vec<&str> = e.questions.iter().map(|q| q.field_key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "question_0",
                "question_0_custom",
                "question_1",
                "question_1_custom"
            ]
        );
    }

    #[test]
    fn parses_number_integer_boolean_with_constraints_and_defaults() {
        let schema = ElicitationSchema::new()
            .property(
                "question_0",
                NumberPropertySchema::new()
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(0.5),
                true,
            )
            .property(
                "question_1",
                IntegerPropertySchema::new().minimum(1).maximum(10),
                false,
            )
            .property(
                "question_2",
                BooleanPropertySchema::new().default_value(true),
                false,
            );
        let req = form_request(schema, "mixed");
        let e = parse_elicitation(Nonce::new(), &req, Utc::now()).unwrap();

        assert_eq!(e.questions[0].kind, ElicitationFieldKind::Number);
        assert_eq!(e.questions[0].minimum, Some(0.0));
        assert_eq!(e.questions[0].maximum, Some(1.0));
        assert_eq!(e.questions[0].default, Some(AnswerValue::Number(0.5)));

        assert_eq!(e.questions[1].kind, ElicitationFieldKind::Integer);
        assert_eq!(e.questions[1].minimum, Some(1.0));
        assert_eq!(e.questions[1].maximum, Some(10.0));

        assert_eq!(e.questions[2].kind, ElicitationFieldKind::Boolean);
        assert_eq!(e.questions[2].default, Some(AnswerValue::Bool(true)));
    }

    #[test]
    fn parses_string_constraints_and_format() {
        let schema = ElicitationSchema::new().property(
            "question_0",
            StringPropertySchema::email()
                .min_length(3)
                .max_length(64)
                .pattern("^.+@.+$")
                .default_value("a@b.co"),
            true,
        );
        let req = form_request(schema, "email");
        let e = parse_elicitation(Nonce::new(), &req, Utc::now()).unwrap();
        let q = &e.questions[0];
        assert_eq!(q.kind, ElicitationFieldKind::FreeText);
        assert_eq!(q.min_length, Some(3));
        assert_eq!(q.max_length, Some(64));
        assert_eq!(q.pattern.as_deref(), Some("^.+@.+$"));
        assert_eq!(q.format.as_deref(), Some("email"));
        assert_eq!(q.default, Some(AnswerValue::Text("a@b.co".into())));
    }

    #[test]
    fn parses_schema_title_and_description() {
        let schema = ElicitationSchema::new()
            .title("Profile")
            .description("Tell us about yourself")
            .string("question_0", false);
        let req = form_request(schema, "msg");
        let e = parse_elicitation(Nonce::new(), &req, Utc::now()).unwrap();
        assert_eq!(e.title.as_deref(), Some("Profile"));
        assert_eq!(e.description.as_deref(), Some("Tell us about yourself"));
    }

    fn sample_elicitation() -> Elicitation {
        Elicitation {
            nonce: Nonce::new(),
            message: "q".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: vec![
                ElicitationQuestion {
                    title: None,
                    description: None,
                    required: true,
                    kind: ElicitationFieldKind::SingleSelect,
                    options: vec![
                        ElicitationOption {
                            value: "Yes".into(),
                            label: "Yes".into(),
                        },
                        ElicitationOption {
                            value: "No".into(),
                            label: "No".into(),
                        },
                    ],
                    ..empty_question("question_0", ElicitationFieldKind::SingleSelect, true)
                },
                ElicitationQuestion {
                    max_items: Some(1),
                    options: vec![
                        ElicitationOption {
                            value: "a".into(),
                            label: "A".into(),
                        },
                        ElicitationOption {
                            value: "b".into(),
                            label: "B".into(),
                        },
                    ],
                    ..empty_question("tags", ElicitationFieldKind::MultiSelect, false)
                },
            ],
            requested_at: Utc::now(),
            resolved: None,
        }
    }

    fn accept(pairs: Vec<(&str, AnswerValue)>) -> ElicitationResolution {
        ElicitationResolution::Accept {
            answers: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn accept_content(
        e: &Elicitation,
        pairs: Vec<(&str, AnswerValue)>,
    ) -> BTreeMap<String, ElicitationContentValue> {
        match build_response(e, accept(pairs)).unwrap().action {
            ElicitationAction::Accept(a) => a.content.unwrap_or_default(),
            other => panic!("expected accept, got {other:?}"),
        }
    }

    #[test]
    fn build_accept_maps_selected_labels() {
        let e = sample_elicitation();
        let content = accept_content(
            &e,
            vec![
                ("question_0", AnswerValue::Text("Yes".into())),
                ("tags", AnswerValue::List(vec!["a".into()])),
            ],
        );
        assert_eq!(
            content.get("question_0"),
            Some(&ElicitationContentValue::String("Yes".into()))
        );
        assert_eq!(
            content.get("tags"),
            Some(&ElicitationContentValue::StringArray(vec!["a".into()]))
        );
    }

    #[test]
    fn summarize_renders_answers_in_question_order() {
        let e = sample_elicitation();
        let mut answers = BTreeMap::new();
        answers.insert(
            "tags".to_string(),
            AnswerValue::List(vec!["a".into(), "b".into()]),
        );
        answers.insert("question_0".to_string(), AnswerValue::Text("Yes".into()));
        let summary = summarize_answers(&e, &answers);
        // Question order from the form, not BTreeMap key order. Selected
        // values render as their option labels ("a"/"b" -> "A"/"B").
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].question, "question_0");
        assert_eq!(summary[0].answer, "Yes");
        assert_eq!(summary[1].question, "tags");
        assert_eq!(summary[1].answer, "A, B");
    }

    #[test]
    fn summarize_maps_select_values_to_option_labels() {
        // Generic MCP form: value is a machine token, label is human text.
        let mcp = Elicitation {
            nonce: Nonce::new(),
            message: "q".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: vec![ElicitationQuestion {
                options: vec![
                    ElicitationOption {
                        value: "tok_blue".into(),
                        label: "Blue".into(),
                    },
                    // AskUserQuestion-style "label <sep> description": the bare
                    // value is kept, the description dropped from the summary.
                    ElicitationOption {
                        value: "Green".into(),
                        label: "Green \u{2014} the color green".into(),
                    },
                ],
                ..empty_question("color", ElicitationFieldKind::SingleSelect, true)
            }],
            requested_at: Utc::now(),
            resolved: None,
        };
        let mut a = BTreeMap::new();
        a.insert("color".to_string(), AnswerValue::Text("tok_blue".into()));
        assert_eq!(summarize_answers(&mcp, &a)[0].answer, "Blue");
        let mut b = BTreeMap::new();
        b.insert("color".to_string(), AnswerValue::Text("Green".into()));
        assert_eq!(summarize_answers(&mcp, &b)[0].answer, "Green");
    }

    #[test]
    fn summarize_omits_unanswered_and_renders_scalar_kinds() {
        let e = Elicitation {
            nonce: Nonce::new(),
            message: "q".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: vec![
                ElicitationQuestion {
                    title: Some("Your name".into()),
                    ..empty_question("name", ElicitationFieldKind::FreeText, false)
                },
                ElicitationQuestion {
                    title: Some("Enable it".into()),
                    ..empty_question("flag", ElicitationFieldKind::Boolean, false)
                },
                ElicitationQuestion {
                    title: Some("Count".into()),
                    ..empty_question("count", ElicitationFieldKind::Integer, false)
                },
                empty_question("skipped", ElicitationFieldKind::FreeText, false),
            ],
            requested_at: Utc::now(),
            resolved: None,
        };
        let mut answers = BTreeMap::new();
        answers.insert("name".to_string(), AnswerValue::Text("Ada".into()));
        answers.insert("flag".to_string(), AnswerValue::Bool(false));
        answers.insert("count".to_string(), AnswerValue::Integer(3));
        let summary = summarize_answers(&e, &answers);
        assert_eq!(
            summary,
            vec![
                ElicitationAnswer {
                    question: "Your name".into(),
                    answer: "Ada".into()
                },
                ElicitationAnswer {
                    question: "Enable it".into(),
                    answer: "No".into()
                },
                ElicitationAnswer {
                    question: "Count".into(),
                    answer: "3".into()
                },
            ]
        );
    }

    #[test]
    fn build_decline_and_cancel() {
        let e = sample_elicitation();
        assert!(matches!(
            build_response(&e, ElicitationResolution::Decline)
                .unwrap()
                .action,
            ElicitationAction::Decline
        ));
        assert!(matches!(
            build_response(&e, ElicitationResolution::Cancel)
                .unwrap()
                .action,
            ElicitationAction::Cancel
        ));
    }

    #[test]
    fn build_rejects_unknown_field() {
        let e = sample_elicitation();
        assert_eq!(
            build_response(&e, accept(vec![("nope", AnswerValue::Text("x".into()))])),
            Err(ElicitationValidationError::UnknownField("nope".into()))
        );
    }

    #[test]
    fn build_rejects_invalid_option_and_missing_required() {
        let e = sample_elicitation();
        assert_eq!(
            build_response(
                &e,
                accept(vec![("question_0", AnswerValue::Text("Maybe".into()))])
            ),
            Err(ElicitationValidationError::InvalidOption {
                field: "question_0".into(),
                value: "Maybe".into(),
            })
        );
        // question_0 is required; omitting it is a missing-required error.
        assert_eq!(
            build_response(
                &e,
                accept(vec![("tags", AnswerValue::List(vec!["a".into()]))])
            ),
            Err(ElicitationValidationError::MissingRequired(
                "question_0".into()
            ))
        );
    }

    #[test]
    fn build_skips_optional_multiselect_with_min_items_when_blank() {
        // An optional multi-select with min_items must not error when left
        // blank: min_items only constrains an actual selection.
        let mut e = sample_elicitation();
        e.questions[1].min_items = Some(2);
        let content = accept_content(&e, vec![("question_0", AnswerValue::Text("Yes".into()))]);
        assert!(!content.contains_key("tags"));
    }

    #[test]
    fn build_enforces_max_items() {
        let e = sample_elicitation();
        assert_eq!(
            build_response(
                &e,
                accept(vec![
                    ("question_0", AnswerValue::Text("Yes".into())),
                    ("tags", AnswerValue::List(vec!["a".into(), "b".into()])),
                ])
            ),
            Err(ElicitationValidationError::TooManyItems {
                field: "tags".into(),
                max: 1,
            })
        );
    }

    fn number_elicitation(kind: ElicitationFieldKind) -> Elicitation {
        Elicitation {
            questions: vec![ElicitationQuestion {
                minimum: Some(0.0),
                maximum: Some(10.0),
                ..empty_question("question_0", kind, true)
            }],
            ..sample_elicitation()
        }
    }

    #[test]
    fn build_number_accepts_and_range_checks() {
        let e = number_elicitation(ElicitationFieldKind::Number);
        let content = accept_content(&e, vec![("question_0", AnswerValue::Number(2.5))]);
        assert_eq!(
            content.get("question_0"),
            Some(&ElicitationContentValue::Number(2.5))
        );
        assert_eq!(
            build_response(&e, accept(vec![("question_0", AnswerValue::Number(99.0))])),
            Err(ElicitationValidationError::OutOfRange {
                field: "question_0".into()
            })
        );
    }

    #[test]
    fn build_integer_coerces_whole_float_and_rejects_fraction() {
        let e = number_elicitation(ElicitationFieldKind::Integer);
        let content = accept_content(&e, vec![("question_0", AnswerValue::Number(3.0))]);
        assert_eq!(
            content.get("question_0"),
            Some(&ElicitationContentValue::Integer(3))
        );
        assert_eq!(
            build_response(&e, accept(vec![("question_0", AnswerValue::Number(3.5))])),
            Err(ElicitationValidationError::NotAnInteger {
                field: "question_0".into()
            })
        );
        // A whole float past i64 range must not saturate-cast past the
        // range check; it is rejected as out of range.
        assert_eq!(
            build_response(&e, accept(vec![("question_0", AnswerValue::Number(1e30))])),
            Err(ElicitationValidationError::OutOfRange {
                field: "question_0".into()
            })
        );
    }

    #[test]
    fn build_boolean_round_trips() {
        let e = Elicitation {
            questions: vec![empty_question(
                "question_0",
                ElicitationFieldKind::Boolean,
                true,
            )],
            ..sample_elicitation()
        };
        let content = accept_content(&e, vec![("question_0", AnswerValue::Bool(true))]);
        assert_eq!(
            content.get("question_0"),
            Some(&ElicitationContentValue::Boolean(true))
        );
    }

    #[test]
    fn build_enforces_string_length_and_pattern() {
        let e = Elicitation {
            questions: vec![ElicitationQuestion {
                min_length: Some(2),
                max_length: Some(5),
                pattern: Some("^[a-z]+$".into()),
                ..empty_question("question_0", ElicitationFieldKind::FreeText, false)
            }],
            ..sample_elicitation()
        };
        assert_eq!(
            build_response(
                &e,
                accept(vec![("question_0", AnswerValue::Text("a".into()))])
            ),
            Err(ElicitationValidationError::TooShort {
                field: "question_0".into(),
                min: 2
            })
        );
        assert_eq!(
            build_response(
                &e,
                accept(vec![("question_0", AnswerValue::Text("toolong".into()))])
            ),
            Err(ElicitationValidationError::TooLong {
                field: "question_0".into(),
                max: 5
            })
        );
        assert_eq!(
            build_response(
                &e,
                accept(vec![("question_0", AnswerValue::Text("AB".into()))])
            ),
            Err(ElicitationValidationError::PatternMismatch {
                field: "question_0".into()
            })
        );
        // A conforming value passes.
        let content = accept_content(&e, vec![("question_0", AnswerValue::Text("abc".into()))]);
        assert_eq!(
            content.get("question_0"),
            Some(&ElicitationContentValue::String("abc".into()))
        );
    }
}
