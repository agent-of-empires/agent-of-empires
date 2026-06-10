//! Form-mode ACP elicitation, normalized for the structured view.
//!
//! claude-agent-acp (>=0.44) re-enables the built-in `AskUserQuestion`
//! tool only when the client advertises `elicitation.form`, then routes
//! the question(s) to us as an `elicitation/create` request carrying a
//! JSON-Schema form. This module owns the boundary between that raw ACP
//! schema and a clean, web-facing view model:
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

use agent_client_protocol::schema::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
    ElicitationSchema, ElicitationScope, MultiSelectItems, StringPropertySchema,
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
    /// Schema property key (`question_0`, `customAnswer`, ...). Echoed
    /// back verbatim as the answer key in the response content.
    pub field_key: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub kind: ElicitationFieldKind,
    /// Selectable options for `SingleSelect` / `MultiSelect`; empty for
    /// `FreeText`.
    pub options: Vec<ElicitationOption>,
    pub min_items: Option<u64>,
    pub max_items: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationFieldKind {
    /// Plain string input (the AskUserQuestion "custom answer" box).
    FreeText,
    /// Pick exactly one option (rendered as radios).
    SingleSelect,
    /// Pick zero or more options (rendered as checkboxes).
    MultiSelect,
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
    /// A field used a JSON-Schema kind the structured view cannot render
    /// (number/integer/boolean). AskUserQuestion never emits these; they
    /// only arise from MCP-server elicitations, which are out of scope.
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
}

/// The user's decision, as sent by the web client on resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ElicitationResolution {
    /// User submitted the form. `answers` maps each answered field key to
    /// its value (a string for free-text / single-select, a list for
    /// multi-select). Unanswered optional fields may be omitted.
    Accept {
        #[serde(default)]
        answers: BTreeMap<String, AnswerValue>,
    },
    /// User skipped the form (ACP `decline`).
    Decline,
    /// User aborted the agent's tool call (ACP `cancel`).
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnswerValue {
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
/// `question_0..N` and serializes them through a `BTreeMap`, which sorts
/// lexically (`question_10` before `question_2`), so recover the numeric
/// order; non-`question_N` keys (e.g. `customAnswer`) sort after, by key.
fn ordered_fields(
    properties: &BTreeMap<String, ElicitationPropertySchema>,
) -> Vec<(&String, &ElicitationPropertySchema)> {
    fn question_index(key: &str) -> Option<u64> {
        key.strip_prefix("question_").and_then(|n| n.parse().ok())
    }
    let mut fields: Vec<_> = properties.iter().collect();
    fields.sort_by(
        |(a, _), (b, _)| match (question_index(a), question_index(b)) {
            (Some(ai), Some(bi)) => ai.cmp(&bi),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.cmp(b),
        },
    );
    fields
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
        field_key: field_key.to_string(),
        title: s.title.clone(),
        description: s.description.clone(),
        required,
        kind,
        options,
        min_items: None,
        max_items: None,
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
                field_key: field_key.to_string(),
                title: a.title.clone(),
                description: a.description.clone(),
                required,
                kind: ElicitationFieldKind::MultiSelect,
                options,
                min_items: a.min_items,
                max_items: a.max_items,
            })
        }
        // Numbers / integers / booleans only come from MCP-server
        // elicitations, which the structured view does not render.
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
        tool_call_id,
        questions,
        requested_at,
        resolved: None,
    })
}

/// Validate a user resolution against the normalized form and build the
/// ACP response. Accept answers are checked server-side: every key must
/// be a known field, value shapes must match the field kind, selected
/// values must be offered options, and required / min / max constraints
/// must hold. This is the only place answers cross back to the agent, so
/// the browser is never trusted to send well-formed content.
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
                    Some(AnswerValue::Text(_)) => {
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
                let text = match answer {
                    Some(AnswerValue::Text(text)) => text.clone(),
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
                    continue;
                }
                content.insert(
                    question.field_key.clone(),
                    ElicitationContentValue::String(text),
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
    use agent_client_protocol::schema::{
        BooleanPropertySchema, ElicitationFormMode, ElicitationSessionScope, EnumOption,
        MultiSelectPropertySchema, StringPropertySchema,
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
    fn rejects_unsupported_field_kind() {
        let schema =
            ElicitationSchema::new().property("question_0", BooleanPropertySchema::new(), false);
        let req = form_request(schema, "bool");
        assert_eq!(
            parse_elicitation(Nonce::new(), &req, Utc::now()),
            Err(ElicitationParseError::UnsupportedField("question_0".into()))
        );
    }

    fn sample_elicitation() -> Elicitation {
        Elicitation {
            nonce: Nonce::new(),
            message: "q".into(),
            tool_call_id: None,
            questions: vec![
                ElicitationQuestion {
                    field_key: "question_0".into(),
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
                    min_items: None,
                    max_items: None,
                },
                ElicitationQuestion {
                    field_key: "tags".into(),
                    title: None,
                    description: None,
                    required: false,
                    kind: ElicitationFieldKind::MultiSelect,
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
                    min_items: None,
                    max_items: Some(1),
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

    #[test]
    fn build_accept_maps_selected_labels() {
        let e = sample_elicitation();
        let resp = build_response(
            &e,
            accept(vec![
                ("question_0", AnswerValue::Text("Yes".into())),
                ("tags", AnswerValue::List(vec!["a".into()])),
            ]),
        )
        .unwrap();
        match resp.action {
            ElicitationAction::Accept(a) => {
                let content = a.content.unwrap();
                assert_eq!(
                    content.get("question_0"),
                    Some(&ElicitationContentValue::String("Yes".into()))
                );
                assert_eq!(
                    content.get("tags"),
                    Some(&ElicitationContentValue::StringArray(vec!["a".into()]))
                );
            }
            other => panic!("expected accept, got {other:?}"),
        }
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
        let resp = build_response(
            &e,
            accept(vec![("question_0", AnswerValue::Text("Yes".into()))]),
        )
        .unwrap();
        match resp.action {
            ElicitationAction::Accept(a) => {
                assert!(!a.content.unwrap_or_default().contains_key("tags"));
            }
            other => panic!("expected accept, got {other:?}"),
        }
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
}
