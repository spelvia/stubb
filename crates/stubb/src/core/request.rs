//! Questions an agent asks the host application during a turn, such as
//! permission to use a tool, and the host's answers.
//!
//! Requests are forwarded only when a session is started or resumed with
//! [`Settings::forward_requests`](crate::session::Settings::forward_requests).
//! The agent then waits: Stubb reports [`Event::Request`](crate::session::Event::Request),
//! the host answers with [`Session::respond`](crate::Session::respond), and
//! Stubb reports [`Event::RequestResolved`](crate::session::Event::RequestResolved).
//! Without the setting, Stubb declines requests itself and reports
//! [`Event::RequestDeclined`](crate::session::Event::RequestDeclined), so a
//! host that never answers cannot stall a turn.
//!
//! A request can also be withdrawn without an answer, for example when the
//! agent cancels it after an interrupt or its process exits; it is then
//! resolved with [`Outcome::Withdrawn`].
//!
//! # Kinds and the decisions that answer them
//!
//! | [`Kind`] | Answer with |
//! |---|---|
//! | [`Kind::Tool`] | [`Decision::Allow`], [`Decision::AllowForSession`], [`Decision::Deny`], [`Decision::Cancel`] |
//! | [`Kind::Permission`] | [`Decision::Select`] with one of its [`Choice`]s, or the decisions of `Tool` |
//! | [`Kind::Question`] | [`Decision::Answer`], [`Decision::Deny`], [`Decision::Cancel`] |
//! | [`Kind::PlanApproval`] | [`Decision::Allow`] to approve, [`Decision::Select`] to approve with one of its options (such as a permission mode), [`Decision::Deny`] with feedback to keep planning, [`Decision::Cancel`] |
//! | [`Kind::Form`] | [`Decision::Submit`], [`Decision::Deny`] to decline, [`Decision::Cancel`] |
//!
//! A decision that does not fit the request, such as an answer to a tool
//! permission, is [`Error::InvalidDecision`](crate::Error::InvalidDecision)
//! and leaves the request pending. Every kind still accepts
//! [`Decision::Deny`] and [`Decision::Cancel`], and [`Decision::Allow`]
//! where it means something, so a host that only knows tool permissions can
//! still answer requests of a newer kind.
//!
//! # What each provider forwards
//!
//! - **Claude Code**: every `can_use_tool` prompt. Its `AskUserQuestion` tool
//!   is a [`Kind::Question`], answered into the tool's input the way Claude
//!   Code's own prompt does; `ExitPlanMode` is a [`Kind::PlanApproval`] whose
//!   options switch the permission mode (`acceptEdits`, `default`); other
//!   tools are [`Kind::Tool`].
//! - **Codex**: command, file-change and permission approvals as
//!   [`Kind::Tool`], `request_user_input` as [`Kind::Question`], and MCP
//!   elicitations as [`Kind::Form`].
//! - **ACP agents**: permission requests as [`Kind::Permission`] with the
//!   agent's own options, and `elicitation/create` as [`Kind::Form`].
//! - **Pi** asks nothing.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A question waiting for [`Session::respond`](crate::Session::respond).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// Pass this to [`Session::respond`](crate::Session::respond). Unique
    /// within the session while the request is pending.
    pub id: String,
    /// The turn the agent was working on, when known.
    pub turn_id: Option<String>,
    pub kind: Kind,
    /// The provider's request as received, for details Stubb does not map.
    pub native: Value,
}

/// What the agent is asking for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Kind {
    /// Permission to call a tool with the given input.
    Tool {
        /// Native tool name, such as `Write` or `Bash`.
        name: String,
        /// Name to show people, when the provider reports one.
        display_name: Option<String>,
        /// Short description of this call, when the provider reports one.
        description: Option<String>,
        input: Value,
        /// The tool call's item id in the turn, when known.
        tool_call_id: Option<String>,
        /// Whether [`Decision::AllowForSession`] is available: the provider
        /// suggested a rule that allows calls like this one from now on.
        can_allow_for_session: bool,
    },
    /// Permission to call a tool, with the choices the agent offers, such as
    /// ACP's "allow once" and "always allow". Answer with
    /// [`Decision::Select`].
    Permission(Permission),
    /// Questions for the user, each with options to pick from and often a
    /// free-text answer. Answer with [`Decision::Answer`].
    Question(Questions),
    /// The agent finished planning and asks to go ahead with its plan.
    PlanApproval(PlanApproval),
    /// Structured input for a form, such as an MCP server's elicitation.
    /// Answer with [`Decision::Submit`].
    Form(Form),
    /// A request kind this version of Stubb does not know, kept as it was
    /// read (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
}

/// A tool permission with the agent's own choices: [`Kind::Permission`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Permission {
    /// Native tool name or kind, such as `edit` or `execute`.
    pub name: String,
    /// Title to show people, when the agent reports one.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input: Value,
    /// The tool call's item id in the turn, when known.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// The choices the agent offers, in its order. Answer with
    /// [`Decision::Select`] and a choice's `id`.
    #[serde(default)]
    pub options: Vec<Choice>,
}

/// One choice an agent offers, picked with [`Decision::Select`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Choice {
    /// Pass this as [`Decision::Select`]'s `option_id`.
    pub id: String,
    /// Text for the button, as the agent wrote it.
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    /// What picking it means, so a host can style it.
    #[serde(default)]
    pub kind: ChoiceKind,
}

impl Choice {
    pub(crate) fn new(id: impl Into<String>, label: impl Into<String>, kind: ChoiceKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            kind,
        }
    }
}

/// What picking a [`Choice`] means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ChoiceKind {
    /// Allow this once.
    AllowOnce,
    /// Allow this and similar calls from now on.
    AllowAlways,
    /// Refuse this once.
    RejectOnce,
    /// Refuse this and similar calls from now on.
    RejectAlways,
    /// A kind this version of Stubb does not know, or none (deserialization
    /// only).
    #[default]
    #[serde(other)]
    Unknown,
}

/// Questions for the user: [`Kind::Question`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Questions {
    pub questions: Vec<Question>,
    /// The tool call that asks them, when the agent asks through a tool.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// One question of a [`Kind::Question`] request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Question {
    /// Identifies the question in [`Answer::question_id`]. Unique within
    /// the request.
    pub id: String,
    /// A short label for the question, such as `Library`.
    #[serde(default)]
    pub header: Option<String>,
    /// The question itself.
    pub question: String,
    /// The answers to pick from, in the agent's order. Empty for a question
    /// answered only with text.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// More than one option may be picked.
    #[serde(default)]
    pub multi_select: bool,
    /// The user may answer in their own words instead of, or as well as,
    /// picking options ([`Answer::other`]).
    #[serde(default)]
    pub allow_other: bool,
    /// The answer is secret, such as a password, and should be masked.
    #[serde(default)]
    pub secret: bool,
}

/// An answer to pick for a [`Question`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuestionOption {
    /// The option's text; [`Answer::selected`] names options by it.
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Content to show when the option is focused, such as a mockup or a
    /// code snippet (Markdown), when the agent provides it.
    #[serde(default)]
    pub preview: Option<String>,
}

/// The agent's plan, waiting for approval: [`Kind::PlanApproval`].
///
/// [`Decision::Allow`] approves it, [`Decision::Select`] approves it with one
/// of `options`, and [`Decision::Deny`] rejects it; its message is passed to
/// the agent as feedback and the agent keeps planning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PlanApproval {
    /// The plan, in Markdown.
    pub plan: String,
    /// Ways to approve it that change how the agent goes on, such as Claude
    /// Code's permission modes. Empty when the agent offers none.
    #[serde(default)]
    pub options: Vec<Choice>,
    /// The file the agent saved the plan to, when it reports one.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// The tool call that asks for approval, when known.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// A form to fill in: [`Kind::Form`], such as an MCP server's elicitation.
///
/// A form either has `fields`, answered with [`Decision::Submit`] and their
/// values, or a `url` the user should open; [`Decision::Submit`] with no
/// values then says they did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Form {
    /// What the form is for.
    pub message: String,
    /// Who asks, such as the MCP server's name, when known.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub fields: Vec<Field>,
    /// A page to open instead of filling in fields.
    #[serde(default)]
    pub url: Option<String>,
    /// The requested JSON schema as received, for constraints Stubb does not
    /// map. `null` when the form has none.
    #[serde(default)]
    pub schema: Value,
    /// The tool call this form belongs to, when known.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// One field of a [`Form`]: a flat JSON-schema property.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Field {
    /// The key of its value in [`Decision::Submit`]'s `values`.
    pub name: String,
    pub kind: FieldKind,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    /// The choices of a [`FieldKind::Select`] or [`FieldKind::MultiSelect`]
    /// field.
    #[serde(default)]
    pub options: Vec<FieldOption>,
    /// The format of a text field: `email`, `uri`, `date` or `date-time`.
    #[serde(default)]
    pub format: Option<String>,
    /// Bounds of a number: its value, of text: its length, of a
    /// multi-select: how many are picked.
    #[serde(default)]
    pub minimum: Option<f64>,
    #[serde(default)]
    pub maximum: Option<f64>,
    #[serde(default)]
    pub default: Option<Value>,
}

/// The value a [`Field`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum FieldKind {
    /// A string.
    Text,
    /// A number.
    Number,
    /// A whole number.
    Integer,
    Boolean,
    /// One of `options`, by value: a string.
    Select,
    /// Any of `options`, by value: an array of strings.
    MultiSelect,
    /// A kind this version of Stubb does not know (deserialization only), or
    /// a property it cannot map; see [`Form::schema`].
    #[serde(other)]
    Unknown,
}

/// A choice of a select [`Field`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FieldOption {
    /// The value to submit.
    pub value: String,
    /// Text to show; the value when the schema has no title for it.
    pub label: String,
}

/// The user's answer to one [`Question`], for [`Decision::Answer`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Answer {
    /// The [`Question::id`] it answers.
    pub question_id: String,
    /// Labels of the picked [`QuestionOption`]s.
    #[serde(default)]
    pub selected: Vec<String>,
    /// The user's own words, for a question that allows them.
    #[serde(default)]
    pub other: Option<String>,
}

impl Answer {
    /// An answer picking `labels`.
    pub fn selected(
        question_id: impl Into<String>,
        labels: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            question_id: question_id.into(),
            selected: labels.into_iter().map(Into::into).collect(),
            other: None,
        }
    }

    /// An answer in the user's own words.
    pub fn other(question_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            question_id: question_id.into(),
            selected: Vec::new(),
            other: Some(text.into()),
        }
    }

    /// Adds the user's own words to an answer.
    #[must_use]
    pub fn with_other(mut self, text: impl Into<String>) -> Self {
        self.other = Some(text.into());
        self
    }

    /// The picked labels, then the user's own words.
    pub(crate) fn values(&self) -> Vec<String> {
        self.selected
            .iter()
            .cloned()
            .chain(self.other.iter().filter(|text| !text.is_empty()).cloned())
            .collect()
    }
}

/// The host's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Decision {
    /// Allow this call once.
    Allow,
    /// Allow this call and apply the provider's own suggested rule for
    /// similar calls in this session. Claude Code applies its
    /// `permission_suggestions`. Only valid when the request reports
    /// `can_allow_for_session`.
    AllowForSession,
    /// Refuse this call; the agent continues without it. The message, when
    /// given, is passed to the agent as the reason.
    Deny { message: Option<String> },
    /// Refuse this call and stop the turn.
    Cancel,
    /// Answers a [`Kind::Question`]: one [`Answer`] per question answered.
    Answer { answers: Vec<Answer> },
    /// Picks one of the request's choices by [`Choice::id`]: a
    /// [`Kind::Permission`] option, or a [`Kind::PlanApproval`] option.
    Select { option_id: String },
    /// Fills in a [`Kind::Form`]: values by [`Field::name`].
    Submit { values: Map<String, Value> },
}

impl Decision {
    /// Whether the call was refused.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Deny { .. } | Self::Cancel)
    }

    /// Its JSON `type`, for errors.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowForSession => "allowForSession",
            Self::Deny { .. } => "deny",
            Self::Cancel => "cancel",
            Self::Answer { .. } => "answer",
            Self::Select { .. } => "select",
            Self::Submit { .. } => "submit",
        }
    }
}

/// How a request stopped being pending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Outcome {
    /// The host answered with [`Session::respond`](crate::Session::respond).
    Answered { decision: Decision },
    /// The request ended without an answer: the agent cancelled it, or the
    /// session closed.
    Withdrawn,
    /// An outcome this version of Stubb does not know, kept as it was read
    /// (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
}

impl Form {
    /// A form from an MCP-style elicitation: a message and a flat object
    /// schema (`requestedSchema`), or a URL to open.
    pub(crate) fn from_schema(message: String, schema: Value) -> Self {
        Self {
            message,
            source: None,
            fields: fields(&schema),
            url: None,
            schema,
            tool_call_id: None,
        }
    }
}

/// The fields of a flat JSON object schema, as MCP elicitation and ACP use:
/// string, number, integer and boolean properties, single selects (`enum` or
/// `oneOf`) and multi-selects (an array of `enum` or `anyOf` items).
fn fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties
        .iter()
        .map(|(name, property)| field(name, property, required.contains(&name.as_str())))
        .collect()
}

fn field(name: &str, property: &Value, required: bool) -> Field {
    let text = |key: &str| property.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |keys: &[&str]| keys.iter().find_map(|key| property.get(*key)?.as_f64());
    let mut field = Field {
        name: name.to_owned(),
        kind: FieldKind::Unknown,
        title: text("title"),
        description: text("description"),
        required,
        options: Vec::new(),
        format: text("format"),
        minimum: None,
        maximum: None,
        default: property.get("default").cloned(),
    };
    match property.get("type").and_then(Value::as_str) {
        Some("string") => {
            field.options = select_options(property);
            if field.options.is_empty() {
                field.kind = FieldKind::Text;
                field.minimum = number(&["minLength"]);
                field.maximum = number(&["maxLength"]);
            } else {
                field.kind = FieldKind::Select;
            }
        }
        Some(kind @ ("number" | "integer")) => {
            field.kind = if kind == "integer" {
                FieldKind::Integer
            } else {
                FieldKind::Number
            };
            field.minimum = number(&["minimum"]);
            field.maximum = number(&["maximum"]);
        }
        Some("boolean") => field.kind = FieldKind::Boolean,
        Some("array") => {
            if let Some(items) = property.get("items") {
                field.options = select_options(items);
            }
            if !field.options.is_empty() {
                field.kind = FieldKind::MultiSelect;
                field.minimum = number(&["minItems"]);
                field.maximum = number(&["maxItems"]);
            }
        }
        _ => {}
    }
    field
}

/// Choices from `enum` (with optional `enumNames`), `oneOf` or `anyOf`.
fn select_options(schema: &Value) -> Vec<FieldOption> {
    let titled = ["oneOf", "anyOf"]
        .iter()
        .find_map(|key| schema.get(*key)?.as_array());
    if let Some(options) = titled {
        return options
            .iter()
            .filter_map(|option| {
                let value = option.get("const")?.as_str()?.to_owned();
                let label = option
                    .get("title")
                    .and_then(Value::as_str)
                    .map_or_else(|| value.clone(), str::to_owned);
                Some(FieldOption { value, label })
            })
            .collect();
    }
    let names = schema.get("enumNames").and_then(Value::as_array);
    schema
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, value)| {
            let value = value.as_str()?.to_owned();
            let label = names
                .and_then(|names| names.get(index)?.as_str())
                .map_or_else(|| value.clone(), str::to_owned);
            Some(FieldOption { value, label })
        })
        .collect()
}

/// [`crate::Error::InvalidDecision`] for request `id`.
pub(crate) fn invalid(id: &str, reason: impl Into<String>) -> crate::Error {
    crate::Error::InvalidDecision {
        id: id.to_owned(),
        reason: reason.into(),
    }
}

/// The option a decision picks from an agent's choices: the selected one,
/// or for the generic decisions the first of a matching kind.
pub(crate) fn pick<'a>(
    id: &str,
    options: &'a [Choice],
    decision: &Decision,
) -> crate::Result<Option<&'a Choice>> {
    let kinds: &[ChoiceKind] = match decision {
        Decision::Select { option_id } => {
            return options
                .iter()
                .find(|choice| &choice.id == option_id)
                .map(Some)
                .ok_or_else(|| invalid(id, format!("the request offers no option {option_id:?}")));
        }
        Decision::Allow => &[ChoiceKind::AllowOnce, ChoiceKind::AllowAlways],
        Decision::AllowForSession => &[ChoiceKind::AllowAlways],
        Decision::Deny { .. } | Decision::Cancel => {
            &[ChoiceKind::RejectOnce, ChoiceKind::RejectAlways]
        }
        _ => return Err(invalid(id, "a permission takes Select, Allow or Deny")),
    };
    Ok(kinds
        .iter()
        .find_map(|kind| options.iter().find(|choice| choice.kind == *kind)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn new_kinds_serialize_flat_with_their_type() {
        let request = Request {
            id: "r".into(),
            turn_id: Some("t".into()),
            kind: Kind::Question(Questions {
                questions: vec![Question {
                    id: "Which library?".into(),
                    header: Some("Library".into()),
                    question: "Which library?".into(),
                    options: vec![QuestionOption {
                        label: "chrono".into(),
                        description: Some("Full-featured".into()),
                        preview: None,
                    }],
                    multi_select: false,
                    allow_other: true,
                    secret: false,
                }],
                tool_call_id: Some("toolu_1".into()),
            }),
            native: json!({}),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "r", "turnId": "t", "native": {},
                "kind": {
                    "type": "question",
                    "toolCallId": "toolu_1",
                    "questions": [{
                        "id": "Which library?", "header": "Library", "question": "Which library?",
                        "options": [{ "label": "chrono", "description": "Full-featured", "preview": null }],
                        "multiSelect": false, "allowOther": true, "secret": false
                    }]
                }
            })
        );
        assert_eq!(serde_json::from_value::<Request>(value).unwrap(), request);
    }

    #[test]
    fn every_kind_and_decision_round_trips() {
        let kinds = [
            json!({ "type": "permission", "name": "edit", "displayName": "Edit a.rs",
                "description": null, "input": {}, "toolCallId": "c",
                "options": [{ "id": "o1", "label": "Allow", "description": null, "kind": "allowOnce" }] }),
            json!({ "type": "planApproval", "plan": "# Plan", "path": "/p.md", "toolCallId": null,
                "options": [{ "id": "acceptEdits", "label": "Yes", "description": null, "kind": "allowOnce" }] }),
            json!({ "type": "form", "message": "Your name?", "source": "github", "url": null,
                "schema": {}, "toolCallId": null, "fields": [{
                    "name": "name", "kind": "text", "title": null, "description": null,
                    "required": true, "options": [], "format": null, "minimum": null,
                    "maximum": null, "default": null }] }),
        ];
        for json in kinds {
            let kind: Kind = serde_json::from_value(json.clone()).unwrap();
            assert!(!matches!(kind, Kind::Unknown(_)), "{json} is a known kind");
            assert_eq!(serde_json::to_value(&kind).unwrap(), json);
        }
        let decisions = [
            json!({ "type": "answer", "answers": [{ "questionId": "q", "selected": ["a"], "other": "b" }] }),
            json!({ "type": "select", "optionId": "o1" }),
            json!({ "type": "submit", "values": { "name": "Ann" } }),
        ];
        for json in decisions {
            let decision: Decision = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(serde_json::to_value(&decision).unwrap(), json);
        }
        // Hosts may leave out what they do not use.
        let decision: Decision = serde_json::from_value(
            json!({ "type": "answer", "answers": [{ "questionId": "q", "other": "mine" }] }),
        )
        .unwrap();
        assert_eq!(
            decision,
            Decision::Answer {
                answers: vec![Answer::other("q", "mine")]
            }
        );
    }

    #[test]
    fn a_known_kind_in_an_unreadable_shape_is_unknown() {
        let kind: Kind =
            serde_json::from_value(json!({ "type": "question", "text": "?" })).unwrap();
        assert!(matches!(kind, Kind::Unknown(_)));
        let kind: ChoiceKind = serde_json::from_value(json!("allowSometimes")).unwrap();
        assert_eq!(kind, ChoiceKind::Unknown);
    }

    #[test]
    fn reads_elicitation_schemas() {
        let form = Form::from_schema(
            "Details".into(),
            json!({
                "type": "object",
                "required": ["email"],
                "properties": {
                    "email": { "type": "string", "format": "email", "title": "Email" },
                    "age": { "type": "integer", "minimum": 0, "maximum": 150 },
                    "ok": { "type": "boolean", "default": true },
                    "size": { "type": "string", "enum": ["s", "m"], "enumNames": ["Small", "Medium"] },
                    "tier": { "type": "string", "oneOf": [{ "const": "pro", "title": "Pro" }] },
                    "tags": { "type": "array", "maxItems": 2, "items": { "anyOf": [{ "const": "a", "title": "A" }] } },
                    "blob": { "type": "object" }
                }
            }),
        );
        let field = |name: &str| form.fields.iter().find(|f| f.name == name).unwrap();
        assert_eq!(field("email").kind, FieldKind::Text);
        assert!(field("email").required);
        assert_eq!(field("email").format.as_deref(), Some("email"));
        assert_eq!(field("age").kind, FieldKind::Integer);
        assert_eq!(field("age").maximum, Some(150.0));
        assert_eq!(field("ok").default, Some(json!(true)));
        assert_eq!(field("size").kind, FieldKind::Select);
        assert_eq!(field("size").options[1].label, "Medium");
        assert_eq!(field("tier").options[0].value, "pro");
        assert_eq!(field("tags").kind, FieldKind::MultiSelect);
        assert_eq!(field("tags").maximum, Some(2.0));
        assert_eq!(field("blob").kind, FieldKind::Unknown);
    }

    #[test]
    fn generic_decisions_pick_an_option_of_their_kind() {
        let options = [
            Choice::new("once", "Allow", ChoiceKind::AllowOnce),
            Choice::new("no", "Reject", ChoiceKind::RejectOnce),
        ];
        let pick = |decision| pick("r", &options, &decision).map(|c| c.map(|c| c.id.clone()));
        assert_eq!(pick(Decision::Allow).unwrap().as_deref(), Some("once"));
        assert_eq!(pick(Decision::AllowForSession).unwrap(), None);
        assert_eq!(pick(Decision::Cancel).unwrap().as_deref(), Some("no"));
        assert_eq!(
            pick(Decision::Select {
                option_id: "no".into()
            })
            .unwrap()
            .as_deref(),
            Some("no")
        );
        assert!(matches!(
            pick(Decision::Select {
                option_id: "maybe".into()
            }),
            Err(crate::Error::InvalidDecision { .. })
        ));
        assert!(pick(Decision::Answer { answers: vec![] }).is_err());
    }
}
