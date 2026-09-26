//! Form-mode ACP elicitation, normalized for the structured view.

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
    ElicitationSchema, ElicitationScope, EnumOption, MultiSelectItems, StringFormat,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::approvals::Nonce;

/// A pending or resolved elicitation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Elicitation {
    pub nonce: Nonce,
    /// Human-readable prompt.
    pub message: String,
    /// Optional schema-level title (MCP elicitations may set one;
    /// AskUserQuestion does not).
    pub title: Option<String>,
    /// Optional schema-level description, rendered under the message.
    pub description: Option<String>,
    /// Tool call this elicitation belongs to, when the agent scoped it to
    /// one.
    pub tool_call_id: Option<String>,
    pub questions: Vec<ElicitationQuestion>,
    pub requested_at: DateTime<Utc>,
    pub resolved: Option<ResolvedElicitation>,
}

/// One field of the elicitation form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationQuestion {
    /// Schema property key (`question_0`, `question_0_custom`, ...).
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
    /// Value echoed back to the agent.
    pub value: String,
    /// Human-readable label.
    pub label: String,
    /// Optional per-option description (e.g. AskUserQuestion's pros/cons
    /// text).
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedElicitation {
    pub outcome: ElicitationOutcome,
    pub resolved_at: DateTime<Utc>,
}

/// One answered question, rendered for the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationAnswer {
    pub question: String,
    pub answer: String,
}

/// Separator the claude-agent-acp adapter wedges between an AskUserQuestion
/// option's label and its description when flattening into the enum title
/// (`"<label> <sep> <description>"`).
const OPTION_DESC_SEP: &str = " \u{2014} ";

/// Render the user's submitted answers into display-ready pairs, in the
/// form's question order.
pub fn summarize_answers(
    elicitation: &Elicitation,
    answers: &BTreeMap<String, AnswerValue>,
) -> Vec<ElicitationAnswer> {
    let mut out = Vec::new();
    for question in &elicitation.questions {
        let Some(value) = answers.get(&question.field_key) else {
            continue;
        };
        // Map a selected option value to its human label.
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
    /// (daemon restart, agent cancel).
    Cancelled,
}

/// Reason a form schema could not be normalized for the structured view.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ElicitationParseError {
    /// URL-mode elicitation.
    #[error("elicitation is not form-mode")]
    NotFormMode,
    /// A field used a JSON-Schema kind the structured view cannot render.
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
    /// User submitted the form.
    Accept {
        #[serde(default)]
        answers: BTreeMap<String, AnswerValue>,
    },
    /// User skipped the form (ACP `decline`).
    Decline,
    /// User aborted the agent's tool call (ACP `cancel`).
    Cancel,
}

/// A submitted (or default) answer value.
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

/// Order a form's properties for display.
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
/// `date-time`) so the web can map it to an input type.
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

fn titled_options(options: &[EnumOption]) -> Vec<ElicitationOption> {
    options
        .iter()
        .map(|o| ElicitationOption {
            value: o.value.clone(),
            label: o.title.clone(),
            description: o.description.clone(),
        })
        .collect()
}

fn bare_options(values: &[String]) -> Vec<ElicitationOption> {
    values
        .iter()
        .map(|v| ElicitationOption {
            value: v.clone(),
            label: v.clone(),
            description: None,
        })
        .collect()
}

fn parse_field(
    field_key: &str,
    prop: &ElicitationPropertySchema,
    required: bool,
) -> Result<ElicitationQuestion, ElicitationParseError> {
    use ElicitationFieldKind as Kind;
    let question = match prop {
        ElicitationPropertySchema::String(s) => {
            // `oneOf` carries titled options, `enum` bare values, neither free text.
            let (kind, options) = match (&s.one_of, &s.enum_values) {
                (Some(one_of), _) => (Kind::SingleSelect, titled_options(one_of)),
                (None, Some(values)) => (Kind::SingleSelect, bare_options(values)),
                (None, None) => (Kind::FreeText, Vec::new()),
            };
            ElicitationQuestion {
                title: s.title.clone(),
                description: s.description.clone(),
                options,
                min_length: s.min_length,
                max_length: s.max_length,
                pattern: s.pattern.clone(),
                format: s.format.as_ref().map(format_token),
                default: s.default.clone().map(AnswerValue::Text),
                ..empty_question(field_key, kind, required)
            }
        }
        ElicitationPropertySchema::Array(a) => ElicitationQuestion {
            title: a.title.clone(),
            description: a.description.clone(),
            options: match &a.items {
                MultiSelectItems::Titled(t) => titled_options(&t.options),
                MultiSelectItems::String(u) => bare_options(&u.values),
                _ => Vec::new(),
            },
            min_items: a.min_items,
            max_items: a.max_items,
            default: a.default.clone().map(AnswerValue::List),
            ..empty_question(field_key, Kind::MultiSelect, required)
        },
        ElicitationPropertySchema::Number(n) => ElicitationQuestion {
            title: n.title.clone(),
            description: n.description.clone(),
            minimum: n.minimum,
            maximum: n.maximum,
            default: n.default.map(AnswerValue::Number),
            ..empty_question(field_key, Kind::Number, required)
        },
        ElicitationPropertySchema::Integer(i) => ElicitationQuestion {
            title: i.title.clone(),
            description: i.description.clone(),
            minimum: i.minimum.map(|v| v as f64),
            maximum: i.maximum.map(|v| v as f64),
            default: i.default.map(AnswerValue::Integer),
            ..empty_question(field_key, Kind::Integer, required)
        },
        ElicitationPropertySchema::Boolean(b) => ElicitationQuestion {
            title: b.title.clone(),
            description: b.description.clone(),
            default: b.default.map(AnswerValue::Bool),
            ..empty_question(field_key, Kind::Boolean, required)
        },
        // The schema enum is non_exhaustive.
        _ => {
            return Err(ElicitationParseError::UnsupportedField(
                field_key.to_string(),
            ))
        }
    };
    Ok(question)
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

/// The string to store for a free-text or single-select answer; `None`
/// when an optional field was left blank.
fn validate_text(
    question: &ElicitationQuestion,
    answer: &AnswerValue,
) -> Result<Option<String>, ElicitationValidationError> {
    let field = || question.field_key.clone();
    let text = match answer {
        AnswerValue::Text(text) => text.clone(),
        // Scalars coerce so free text stays forgiving; selects are checked below.
        AnswerValue::Integer(i) => i.to_string(),
        AnswerValue::Number(n) => n.to_string(),
        AnswerValue::Bool(b) => b.to_string(),
        AnswerValue::List(_) => {
            return Err(ElicitationValidationError::WrongValueType(
                field(),
                "string",
            ))
        }
    };
    if question.kind == ElicitationFieldKind::SingleSelect
        && !text.is_empty()
        && !question.options.iter().any(|o| o.value == text)
    {
        return Err(ElicitationValidationError::InvalidOption {
            field: field(),
            value: text,
        });
    }
    if text.is_empty() {
        return missing(question);
    }
    // Length and pattern bound typed text only, never a fixed option.
    if question.kind == ElicitationFieldKind::FreeText {
        let len = text.chars().count() as u32;
        if let Some(min) = question.min_length.filter(|&min| len < min) {
            return Err(ElicitationValidationError::TooShort {
                field: field(),
                min,
            });
        }
        if let Some(max) = question.max_length.filter(|&max| len > max) {
            return Err(ElicitationValidationError::TooLong {
                field: field(),
                max,
            });
        }
        // An invalid pattern from the agent is no constraint rather than a wall.
        let mismatch = question
            .pattern
            .as_deref()
            .and_then(|p| regex::Regex::new(p).ok())
            .is_some_and(|re| !re.is_match(&text));
        if mismatch {
            return Err(ElicitationValidationError::PatternMismatch { field: field() });
        }
    }
    Ok(Some(text))
}

fn missing<T>(question: &ElicitationQuestion) -> Result<Option<T>, ElicitationValidationError> {
    if question.required {
        return Err(ElicitationValidationError::MissingRequired(
            question.field_key.clone(),
        ));
    }
    Ok(None)
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

/// Validate one answer against its question; `None` for an unanswered optional field.
fn field_content(
    question: &ElicitationQuestion,
    answer: Option<&AnswerValue>,
) -> Result<Option<ElicitationContentValue>, ElicitationValidationError> {
    use ElicitationValidationError as E;
    let field = || question.field_key.clone();
    let Some(answer) = answer else {
        return missing(question);
    };
    let wrong = |ty| Err(E::WrongValueType(field(), ty));
    let value = match question.kind {
        ElicitationFieldKind::MultiSelect => {
            let AnswerValue::List(selected) = answer else {
                return wrong("list");
            };
            if let Some(value) = selected
                .iter()
                .find(|v| !question.options.iter().any(|o| &o.value == *v))
            {
                return Err(E::InvalidOption {
                    field: field(),
                    value: value.clone(),
                });
            }
            if selected.is_empty() {
                return missing(question);
            }
            let n = selected.len() as u64;
            if let Some(min) = question.min_items.filter(|&min| n < min) {
                return Err(E::TooFewItems {
                    field: field(),
                    min,
                });
            }
            if let Some(max) = question.max_items.filter(|&max| n > max) {
                return Err(E::TooManyItems {
                    field: field(),
                    max,
                });
            }
            ElicitationContentValue::StringArray(selected.clone())
        }
        ElicitationFieldKind::SingleSelect | ElicitationFieldKind::FreeText => {
            match validate_text(question, answer)? {
                Some(text) => ElicitationContentValue::String(text),
                None => return Ok(None),
            }
        }
        ElicitationFieldKind::Number => {
            let value = match answer {
                AnswerValue::Number(n) => *n,
                AnswerValue::Integer(i) => *i as f64,
                _ => return wrong("number"),
            };
            check_range(question, value)?;
            ElicitationContentValue::Number(value)
        }
        ElicitationFieldKind::Integer => {
            let value = match answer {
                AnswerValue::Integer(i) => *i,
                // The browser may send a whole number as a JSON float.
                AnswerValue::Number(n)
                    if n.is_finite()
                        && n.fract() == 0.0
                        && *n >= i64::MIN as f64
                        && *n <= i64::MAX as f64 =>
                {
                    *n as i64
                }
                AnswerValue::Number(n) if n.is_finite() && n.fract() != 0.0 => {
                    return Err(E::NotAnInteger { field: field() });
                }
                AnswerValue::Number(_) => return Err(E::OutOfRange { field: field() }),
                _ => return wrong("integer"),
            };
            check_range(question, value as f64)?;
            ElicitationContentValue::Integer(value)
        }
        ElicitationFieldKind::Boolean => {
            let AnswerValue::Bool(value) = answer else {
                return wrong("boolean");
            };
            ElicitationContentValue::Boolean(*value)
        }
    };
    Ok(Some(value))
}

/// Validate a user resolution against the normalized form and build the ACP
/// response; the browser is never trusted to send well-formed answers.
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
    if let Some(key) = answers
        .keys()
        .find(|key| !elicitation.questions.iter().any(|q| &q.field_key == *key))
    {
        return Err(ElicitationValidationError::UnknownField(key.clone()));
    }
    let mut content = BTreeMap::new();
    for question in &elicitation.questions {
        if let Some(value) = field_content(question, answers.get(&question.field_key))? {
            content.insert(question.field_key.clone(), value);
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
        BooleanPropertySchema, ElicitationFormMode, ElicitationSessionScope, IntegerPropertySchema,
        MultiSelectPropertySchema, NumberPropertySchema, StringPropertySchema,
    };
    use ElicitationFieldKind as Kind;
    use ElicitationValidationError as E;

    fn parse(schema: ElicitationSchema) -> Elicitation {
        let request = CreateElicitationRequest::new(
            ElicitationFormMode::new(ElicitationSessionScope::new("sess-1"), schema),
            "Pick one?",
        );
        parse_elicitation(Nonce::new(), &request, Utc::now()).unwrap()
    }

    fn option(value: &str, label: &str) -> ElicitationOption {
        ElicitationOption {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    fn form(questions: Vec<ElicitationQuestion>) -> Elicitation {
        Elicitation {
            nonce: Nonce::new(),
            message: "q".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions,
            requested_at: Utc::now(),
            resolved: None,
        }
    }

    /// A required yes/no select plus an optional multi-select of at most one tag.
    fn sample_form() -> Elicitation {
        form(vec![
            ElicitationQuestion {
                options: vec![option("Yes", "Yes"), option("No", "No")],
                ..empty_question("question_0", Kind::SingleSelect, true)
            },
            ElicitationQuestion {
                max_items: Some(1),
                options: vec![option("a", "A"), option("b", "B")],
                ..empty_question("tags", Kind::MultiSelect, false)
            },
        ])
    }

    fn answers(pairs: Vec<(&str, AnswerValue)>) -> BTreeMap<String, AnswerValue> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn respond(
        e: &Elicitation,
        pairs: Vec<(&str, AnswerValue)>,
    ) -> Result<BTreeMap<String, ElicitationContentValue>, ElicitationValidationError> {
        let resolution = ElicitationResolution::Accept {
            answers: answers(pairs),
        };
        match build_response(e, resolution)?.action {
            ElicitationAction::Accept(a) => Ok(a.content.unwrap_or_default()),
            other => panic!("expected accept, got {other:?}"),
        }
    }

    fn text(s: &str) -> AnswerValue {
        AnswerValue::Text(s.into())
    }

    #[test]
    fn parses_questions_in_order_with_constraints_and_defaults() {
        let e = parse(
            ElicitationSchema::new()
                .title("Profile")
                .description("Tell us about yourself")
                .property(
                    "question_0",
                    StringPropertySchema::new().title("Pick one").one_of(vec![
                        EnumOption::new("Yes", "Yes").description("Ships now"),
                        EnumOption::new("No", "No"),
                    ]),
                    true,
                )
                .property(
                    "question_1",
                    MultiSelectPropertySchema::titled(vec![
                        EnumOption::new("a", "Apple").description("Crisp"),
                        EnumOption::new("b", "Banana"),
                    ]),
                    false,
                )
                .property(
                    "question_2",
                    StringPropertySchema::new().enum_values(vec!["Yes".into(), "No".into()]),
                    false,
                )
                .property(
                    "question_3",
                    MultiSelectPropertySchema::new(vec!["a".into(), "b".into()]),
                    false,
                ),
        );
        assert_eq!(e.message, "Pick one?");
        assert_eq!(e.title.as_deref(), Some("Profile"));
        assert_eq!(e.description.as_deref(), Some("Tell us about yourself"));
        let kinds: Vec<_> = e.questions.iter().map(|q| (q.kind, q.required)).collect();
        assert_eq!(
            kinds,
            [
                (Kind::SingleSelect, true),
                (Kind::MultiSelect, false),
                (Kind::SingleSelect, false),
                (Kind::MultiSelect, false)
            ]
        );
        let descriptions = |i: usize| -> Vec<Option<&str>> {
            e.questions[i]
                .options
                .iter()
                .map(|o| o.description.as_deref())
                .collect()
        };
        assert_eq!(descriptions(0), [Some("Ships now"), None]);
        assert_eq!(descriptions(1), [Some("Crisp"), None]);
        assert_eq!(
            descriptions(2),
            [None, None],
            "bare enums carry no description"
        );
        assert_eq!(descriptions(3), [None, None]);
        assert_eq!(e.questions[1].options[0].label, "Apple");
        assert_eq!(e.questions[2].options[0].label, "Yes");

        let e = parse(
            ElicitationSchema::new()
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
                )
                .property(
                    "question_3",
                    StringPropertySchema::email()
                        .min_length(3)
                        .max_length(64)
                        .pattern("^.+@.+$")
                        .default_value("a@b.co"),
                    true,
                ),
        );
        let [number, integer, boolean, email] = &e.questions[..] else {
            panic!("four questions");
        };
        assert_eq!(
            (number.kind, number.minimum, number.maximum, &number.default),
            (
                Kind::Number,
                Some(0.0),
                Some(1.0),
                &Some(AnswerValue::Number(0.5))
            )
        );
        assert_eq!(
            (integer.kind, integer.minimum, integer.maximum),
            (Kind::Integer, Some(1.0), Some(10.0))
        );
        assert_eq!(
            (boolean.kind, &boolean.default),
            (Kind::Boolean, &Some(AnswerValue::Bool(true)))
        );
        assert_eq!(email.kind, Kind::FreeText);
        assert_eq!((email.min_length, email.max_length), (Some(3), Some(64)));
        assert_eq!(email.pattern.as_deref(), Some("^.+@.+$"));
        assert_eq!(email.format.as_deref(), Some("email"));
        assert_eq!(email.default, Some(text("a@b.co")));

        // Questions order numerically with custom boxes beside them.
        let other = || StringPropertySchema::new().title("Other");
        let e = parse(
            ElicitationSchema::new()
                .property("question_10", MultiSelectPropertySchema::new(vec![]), false)
                .property("customAnswer", other(), false)
                .string("question_2", false)
                .property("question_2_custom", other(), false)
                .string("question_0", false),
        );
        let keys: Vec<&str> = e.questions.iter().map(|q| q.field_key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "question_0",
                "question_2",
                "question_2_custom",
                "question_10",
                "customAnswer"
            ]
        );
        assert_eq!(e.questions[1].kind, Kind::FreeText);
    }

    #[test]
    fn summarize_renders_labels_and_scalars_in_question_order() {
        let e = sample_form();
        let summary = summarize_answers(
            &e,
            &answers(vec![
                ("tags", AnswerValue::List(vec!["a".into(), "b".into()])),
                ("question_0", text("Yes")),
            ]),
        );
        let pairs: Vec<(&str, &str)> = summary
            .iter()
            .map(|a| (a.question.as_str(), a.answer.as_str()))
            .collect();
        assert_eq!(pairs, [("question_0", "Yes"), ("tags", "A, B")]);

        // A machine token maps to its label; an adapter-flattened "value — description"
        // label renders as the bare value.
        let mcp = form(vec![ElicitationQuestion {
            options: vec![
                option("tok_blue", "Blue"),
                option("Green", "Green \u{2014} the color green"),
            ],
            ..empty_question("color", Kind::SingleSelect, true)
        }]);
        for (raw, want) in [("tok_blue", "Blue"), ("Green", "Green")] {
            let summary = summarize_answers(&mcp, &answers(vec![("color", text(raw))]));
            assert_eq!(summary[0].answer, want);
        }

        let titled = |key, title: &str, kind| ElicitationQuestion {
            title: Some(title.into()),
            ..empty_question(key, kind, false)
        };
        let scalars = form(vec![
            titled("name", "Your name", Kind::FreeText),
            titled("flag", "Enable it", Kind::Boolean),
            titled("count", "Count", Kind::Integer),
            empty_question("skipped", Kind::FreeText, false),
        ]);
        let summary = summarize_answers(
            &scalars,
            &answers(vec![
                ("name", text("Ada")),
                ("flag", AnswerValue::Bool(false)),
                ("count", AnswerValue::Integer(3)),
            ]),
        );
        let pairs: Vec<(&str, &str)> = summary
            .iter()
            .map(|a| (a.question.as_str(), a.answer.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [("Your name", "Ada"), ("Enable it", "No"), ("Count", "3")]
        );
    }

    #[test]
    fn build_response_maps_decisions_and_validates_answers() {
        let e = sample_form();
        for (resolution, want) in [
            (ElicitationResolution::Decline, "Decline"),
            (ElicitationResolution::Cancel, "Cancel"),
        ] {
            let action = build_response(&e, resolution).unwrap().action;
            assert!(format!("{action:?}").starts_with(want), "{action:?}");
        }

        let content = respond(
            &e,
            vec![
                ("question_0", text("Yes")),
                ("tags", AnswerValue::List(vec!["a".into()])),
            ],
        )
        .unwrap();
        assert_eq!(
            content,
            BTreeMap::from([
                (
                    "question_0".to_string(),
                    ElicitationContentValue::String("Yes".into())
                ),
                (
                    "tags".to_string(),
                    ElicitationContentValue::StringArray(vec!["a".into()])
                ),
            ])
        );

        let two_tags = AnswerValue::List(vec!["a".into(), "b".into()]);
        let cases = [
            (vec![("nope", text("x"))], E::UnknownField("nope".into())),
            (
                vec![("question_0", text("Maybe"))],
                E::InvalidOption {
                    field: "question_0".into(),
                    value: "Maybe".into(),
                },
            ),
            (
                vec![("tags", AnswerValue::List(vec!["a".into()]))],
                E::MissingRequired("question_0".into()),
            ),
            (
                vec![("question_0", text("Yes")), ("tags", two_tags)],
                E::TooManyItems {
                    field: "tags".into(),
                    max: 1,
                },
            ),
        ];
        for (pairs, want) in cases {
            assert_eq!(respond(&e, pairs), Err(want));
        }

        // An optional multi-select's min_items only bounds a selection actually made.
        let mut e = sample_form();
        e.questions[1].min_items = Some(2);
        let content = respond(&e, vec![("question_0", text("Yes"))]).unwrap();
        assert!(!content.contains_key("tags"));

        let bounded = |kind| {
            form(vec![ElicitationQuestion {
                minimum: Some(0.0),
                maximum: Some(10.0),
                ..empty_question("question_0", kind, true)
            }])
        };
        let string = form(vec![ElicitationQuestion {
            min_length: Some(2),
            max_length: Some(5),
            pattern: Some("^[a-z]+$".into()),
            ..empty_question("question_0", Kind::FreeText, false)
        }]);
        let boolean = form(vec![empty_question("question_0", Kind::Boolean, true)]);
        let (number, integer) = (bounded(Kind::Number), bounded(Kind::Integer));
        let field = || "question_0".to_string();
        // (form, answer, expected content or error)
        let cases: Vec<(
            &Elicitation,
            AnswerValue,
            Result<ElicitationContentValue, E>,
        )> = vec![
            (
                &number,
                AnswerValue::Number(2.5),
                Ok(ElicitationContentValue::Number(2.5)),
            ),
            (
                &number,
                AnswerValue::Number(99.0),
                Err(E::OutOfRange { field: field() }),
            ),
            (
                &integer,
                AnswerValue::Number(3.0),
                Ok(ElicitationContentValue::Integer(3)),
            ),
            (
                &integer,
                AnswerValue::Number(3.5),
                Err(E::NotAnInteger { field: field() }),
            ),
            // A whole float past i64 range must not saturate into range.
            (
                &integer,
                AnswerValue::Number(1e30),
                Err(E::OutOfRange { field: field() }),
            ),
            (
                &boolean,
                AnswerValue::Bool(true),
                Ok(ElicitationContentValue::Boolean(true)),
            ),
            (
                &boolean,
                text("yes"),
                Err(E::WrongValueType(field(), "boolean")),
            ),
            (
                &string,
                text("a"),
                Err(E::TooShort {
                    field: field(),
                    min: 2,
                }),
            ),
            (
                &string,
                text("toolong"),
                Err(E::TooLong {
                    field: field(),
                    max: 5,
                }),
            ),
            (
                &string,
                text("AB"),
                Err(E::PatternMismatch { field: field() }),
            ),
            (
                &string,
                text("abc"),
                Ok(ElicitationContentValue::String("abc".into())),
            ),
        ];
        for (i, (form, answer, want)) in cases.into_iter().enumerate() {
            let got = respond(form, vec![("question_0", answer)])
                .map(|mut content| content.remove("question_0").expect("answered"));
            assert_eq!(got, want, "case {i}");
        }
    }
}
