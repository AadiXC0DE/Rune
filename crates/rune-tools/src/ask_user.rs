//! Asking the user a structured question.
//!
//! Used when a concrete decision blocks progress and no tool can answer it. The
//! question is collected interactively; a noninteractive run reports that input
//! is required rather than waiting or choosing for the user.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};

/// Fewest options a question may offer.
pub const MIN_OPTIONS: usize = 2;

/// Most options a question may offer.
pub const MAX_OPTIONS: usize = 6;

/// Most questions accepted in one call.
pub const MAX_QUESTIONS: usize = 4;

/// Longest question text accepted.
pub const MAX_QUESTION_BYTES: usize = 1024;

/// Longest option label accepted.
pub const MAX_LABEL_BYTES: usize = 128;

/// One selectable option.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Choice {
    /// Short label, one to five words.
    pub label: String,
    /// Optional one-line consequence of choosing it.
    pub description: Option<String>,
}

/// One question.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Question {
    /// The decision being asked about.
    pub text: String,
    /// The available choices.
    pub options: Vec<Choice>,
}

/// Decodes the arguments into questions.
pub fn decode_questions(arguments: &serde_json::Value) -> Result<Vec<Question>> {
    let Some(raw) = arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
    else {
        return Err(RuneError::missing_field("questions"));
    };
    if raw.is_empty() {
        return Err(RuneError::invalid_field("questions", "must not be empty"));
    }
    if raw.len() > MAX_QUESTIONS {
        return Err(RuneError::too_large("questions", raw.len(), MAX_QUESTIONS));
    }

    let mut questions = Vec::with_capacity(raw.len());
    for (index, entry) in raw.iter().enumerate() {
        let text = entry
            .get("question")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| RuneError::missing_field(format!("questions[{index}].question")))?;
        if text.trim().is_empty() {
            return Err(RuneError::invalid_field(
                format!("questions[{index}].question"),
                "must not be empty",
            ));
        }
        if text.len() > MAX_QUESTION_BYTES {
            return Err(RuneError::too_large(
                format!("questions[{index}].question"),
                text.len(),
                MAX_QUESTION_BYTES,
            ));
        }

        let Some(raw_options) = entry.get("options").and_then(serde_json::Value::as_array) else {
            return Err(RuneError::missing_field(format!(
                "questions[{index}].options"
            )));
        };
        if raw_options.len() < MIN_OPTIONS || raw_options.len() > MAX_OPTIONS {
            return Err(RuneError::invalid_field(
                format!("questions[{index}].options"),
                format!(
                    "holds {} options, expected between {MIN_OPTIONS} and {MAX_OPTIONS}",
                    raw_options.len()
                ),
            ));
        }

        let mut options = Vec::with_capacity(raw_options.len());
        for (position, option) in raw_options.iter().enumerate() {
            let label = option
                .get("label")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    RuneError::missing_field(format!(
                        "questions[{index}].options[{position}].label"
                    ))
                })?;
            if label.trim().is_empty() {
                return Err(RuneError::invalid_field(
                    format!("questions[{index}].options[{position}].label"),
                    "must not be empty",
                ));
            }
            if label.len() > MAX_LABEL_BYTES {
                return Err(RuneError::too_large(
                    format!("questions[{index}].options[{position}].label"),
                    label.len(),
                    MAX_LABEL_BYTES,
                ));
            }
            options.push(Choice {
                label: label.trim().to_owned(),
                description: option
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            });
        }

        questions.push(Question {
            text: text.trim().to_owned(),
            options,
        });
    }

    Ok(questions)
}

/// How a question was answered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The user chose an option, by index.
    Chosen(Vec<usize>),
    /// The run was cancelled while waiting.
    Cancelled,
}

/// Answers questions on behalf of a user.
///
/// Behind a trait so the tool can be driven by a terminal, an editor, or a test
/// without the tool knowing which.
pub trait Answerer: Send + Sync {
    /// Collects one answer per question.
    fn ask(&self, questions: &[Question], context: &ExecutionContext) -> Result<Answer>;
}

/// An answerer that always reports that input is unavailable.
///
/// This is what a noninteractive run uses, so a question blocks rather than
/// being answered on the user's behalf.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unavailable;

impl Answerer for Unavailable {
    fn ask(&self, _questions: &[Question], _context: &ExecutionContext) -> Result<Answer> {
        Err(RuneError::new(
            ErrorCode::InputRequired,
            "a question needs an answer, and this run cannot collect one",
        )
        .with_hint("run interactively, or answer in the prompt"))
    }
}

/// An answerer that returns scripted answers, for tests.
#[derive(Debug, Default)]
pub struct Scripted {
    answers: Arc<Mutex<Vec<Answer>>>,
}

impl Scripted {
    /// Builds an answerer from a list of answers.
    #[must_use]
    pub fn new<I>(answers: I) -> Self
    where
        I: IntoIterator<Item = Answer>,
    {
        Self {
            answers: Arc::new(Mutex::new(answers.into_iter().collect())),
        }
    }

    /// Returns how many answers remain.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.answers.lock().map_or(0, |a| a.len())
    }
}

impl Answerer for Scripted {
    fn ask(&self, questions: &[Question], _context: &ExecutionContext) -> Result<Answer> {
        let mut answers = self.answers.lock().map_err(|_| {
            RuneError::new(ErrorCode::Internal, "the scripted answer lock was poisoned")
        })?;
        let answer = answers.remove(0);
        // An out-of-range choice is a defect in the test script rather than a
        // user error, so it is reported rather than clamped.
        if let Answer::Chosen(indices) = &answer
            && indices.len() != questions.len()
        {
            return Err(RuneError::new(
                ErrorCode::Internal,
                format!(
                    "the script supplied {} answers for {} questions",
                    indices.len(),
                    questions.len()
                ),
            ));
        }
        Ok(answer)
    }
}

/// The tool.
pub struct AskUserQuestion {
    answerer: Arc<dyn Answerer>,
}

impl std::fmt::Debug for AskUserQuestion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskUserQuestion").finish_non_exhaustive()
    }
}

impl AskUserQuestion {
    /// Builds the tool with an answerer.
    #[must_use]
    pub fn new(answerer: Arc<dyn Answerer>) -> Self {
        Self { answerer }
    }

    /// Builds the tool for a run that cannot collect an answer.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::new(Arc::new(Unavailable))
    }
}

impl Tool for AskUserQuestion {
    fn name(&self) -> &'static str {
        "ask_user_question"
    }

    fn description(&self) -> &'static str {
        "Ask the user one to four multiple-choice questions when a concrete decision \
         blocks progress and no tool can answer it. Use it for a choice between precise \
         alternatives, not for facts you can look up. Do not use it for safety decisions \
         or for open-ended discussion."
    }

    fn input_schema(&self) -> serde_json::Value {
        // The schema literals mirror the constants above, which is what the
        // decoder enforces; a drift test below keeps the two in step.
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "maxLength": 1024,
                                "description": "The blocking decision, stated as a question."
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 6,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "maxLength": 128,
                                            "description": "Short label, one to five words."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Optional one-line consequence of this choice."
                                        }
                                    },
                                    "required": ["label"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Interact
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        let questions = decode_questions(arguments)?;

        match self.answerer.ask(&questions, context) {
            Ok(Answer::Cancelled) => Ok(ToolOutput::failure(
                "the question was cancelled before it was answered",
            )),
            Ok(Answer::Chosen(indices)) => {
                Ok(ToolOutput::success(render_answers(&questions, &indices)))
            }
            Err(err) if matches!(err.code(), ErrorCode::InputRequired) => {
                // A run that cannot collect an answer returns a typed result
                // rather than waiting, so the model can proceed or report the
                // blocker instead of the process hanging.
                Ok(ToolOutput::failure(format!(
                    "the question could not be asked: {}",
                    err.message()
                )))
            }
            Err(err) if matches!(err.code(), ErrorCode::Cancelled) => Err(err),
            Err(err) => Ok(ToolOutput::failure(err.message().to_owned())),
        }
    }
}

/// Renders the chosen answers for the model.
fn render_answers(questions: &[Question], indices: &[usize]) -> String {
    let mut out = String::new();
    for (question, index) in questions.iter().zip(indices.iter()) {
        let label = question
            .options
            .get(*index)
            .map_or("<invalid choice>", |option| option.label.as_str());
        let _ = writeln!(out, "{}\nAnswer: {label}", question.text);
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn context() -> ExecutionContext {
        ExecutionContext::new(Utf8PathBuf::from("/tmp/workspace"))
    }

    fn question_json() -> serde_json::Value {
        serde_json::json!({
            "questions": [{
                "question": "Which store should the cache use?",
                "options": [
                    { "label": "In memory", "description": "Simplest, lost on restart" },
                    { "label": "On disk", "description": "Survives a restart" }
                ]
            }]
        })
    }

    #[test]
    fn the_tool_reports_its_identity() {
        let tool = AskUserQuestion::unavailable();
        assert_eq!(tool.name(), "ask_user_question");
        assert_eq!(tool.activity(), Activity::Interact);
        assert!(!tool.is_read_only());
    }

    #[test]
    fn the_schema_bounds_match_the_constants() {
        let schema = AskUserQuestion::unavailable().input_schema();
        let questions = &schema["properties"]["questions"];
        assert_eq!(questions["minItems"].as_u64(), Some(1));
        assert_eq!(questions["maxItems"].as_u64(), Some(MAX_QUESTIONS as u64));
        let options = &questions["items"]["properties"]["options"];
        assert_eq!(options["minItems"].as_u64(), Some(MIN_OPTIONS as u64));
        assert_eq!(options["maxItems"].as_u64(), Some(MAX_OPTIONS as u64));
        assert_eq!(
            schema["properties"]["questions"]["items"]["properties"]["question"]["maxLength"]
                .as_u64(),
            Some(MAX_QUESTION_BYTES as u64)
        );
    }

    #[test]
    fn the_schema_requires_questions() {
        let schema = AskUserQuestion::unavailable().input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "questions");
    }

    #[test]
    fn a_well_formed_call_decodes() {
        let questions = decode_questions(&question_json()).expect("decoded");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].options[0].label, "In memory");
        assert!(questions[0].options[0].description.is_some());
    }

    #[test]
    fn a_missing_questions_field_is_rejected() {
        let err = decode_questions(&serde_json::json!({})).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::MissingField);
    }

    #[test]
    fn an_empty_question_list_is_rejected() {
        let err = decode_questions(&serde_json::json!({ "questions": [] })).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn too_many_questions_are_rejected() {
        let questions: Vec<serde_json::Value> = (0..=MAX_QUESTIONS)
            .map(|index| {
                serde_json::json!({
                    "question": format!("q{index}"),
                    "options": [{ "label": "a" }, { "label": "b" }]
                })
            })
            .collect();
        let err =
            decode_questions(&serde_json::json!({ "questions": questions })).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn a_question_with_one_option_is_rejected() {
        let arguments = serde_json::json!({
            "questions": [{ "question": "q", "options": [{ "label": "only" }] }]
        });
        let err = decode_questions(&arguments).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(
            err.message().contains("between 2 and 6"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn a_question_with_too_many_options_is_rejected() {
        let options: Vec<serde_json::Value> = (0..=MAX_OPTIONS)
            .map(|index| serde_json::json!({ "label": format!("o{index}") }))
            .collect();
        let arguments = serde_json::json!({
            "questions": [{ "question": "q", "options": options }]
        });
        assert!(decode_questions(&arguments).is_err());
    }

    #[test]
    fn an_option_without_a_label_is_rejected() {
        let arguments = serde_json::json!({
            "questions": [{ "question": "q", "options": [{ "description": "d" }, { "label": "b" }] }]
        });
        let err = decode_questions(&arguments).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert!(err.message().contains("label"), "{}", err.message());
    }

    #[test]
    fn an_oversized_question_is_rejected() {
        let arguments = serde_json::json!({
            "questions": [{
                "question": "q".repeat(MAX_QUESTION_BYTES + 1),
                "options": [{ "label": "a" }, { "label": "b" }]
            }]
        });
        let err = decode_questions(&arguments).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn a_noninteractive_run_reports_that_input_is_required() {
        let tool = AskUserQuestion::unavailable();
        let output = tool
            .call(&question_json(), &context())
            .expect("a typed failure, not an abort");
        // The result is a tool failure carrying guidance, so the model can
        // adapt instead of the process hanging on input that will not arrive.
        assert!(output.is_error);
        assert!(
            output.text.contains("could not be asked"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_scripted_answer_is_rendered() {
        let tool = AskUserQuestion::new(Arc::new(Scripted::new([Answer::Chosen(vec![1])])));
        let output = tool.call(&question_json(), &context()).expect("answered");
        assert!(!output.is_error);
        assert!(output.text.contains("On disk"), "{}", output.text);
    }

    #[test]
    fn a_cancelled_question_is_reported_as_a_failure() {
        let tool = AskUserQuestion::new(Arc::new(Scripted::new([Answer::Cancelled])));
        let output = tool.call(&question_json(), &context()).expect("cancelled");
        assert!(output.is_error);
        assert!(output.text.contains("cancelled"));
    }

    #[test]
    fn a_cancelled_context_refuses_before_asking() {
        let tool = AskUserQuestion::new(Arc::new(Scripted::new([Answer::Chosen(vec![0])])));
        let context = context();
        context.cancellation().cancel();
        let err = tool
            .call(&question_json(), &context)
            .expect_err("cancelled");
        assert_eq!(err.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn a_malformed_call_is_a_tool_failure_not_an_abort() {
        let tool = AskUserQuestion::new(Arc::new(Scripted::new([Answer::Chosen(vec![0])])));
        let err = tool
            .call(&serde_json::json!({ "questions": [] }), &context())
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_script_with_the_wrong_answer_count_is_a_defect_not_a_guess() {
        let tool = AskUserQuestion::new(Arc::new(Scripted::new([Answer::Chosen(vec![0, 1])])));
        let output = tool.call(&question_json(), &context()).expect("reported");
        // The defect surfaces as a tool failure rather than an answer that does
        // not correspond to the question asked.
        assert!(output.is_error);
    }

    #[test]
    fn the_scripted_answerer_consumes_one_answer_per_call() {
        let answerer = Scripted::new([Answer::Chosen(vec![0]), Answer::Cancelled]);
        assert_eq!(answerer.remaining(), 2);
        let tool = AskUserQuestion::new(Arc::new(answerer));
        tool.call(&question_json(), &context()).expect("first");
        // The first answer was consumed, which the second call demonstrates by
        // returning the cancelled answer rather than the chosen one.
        let second = tool.call(&question_json(), &context()).expect("second");
        assert!(second.is_error, "the same answer was used twice");
    }

    #[test]
    fn an_out_of_range_choice_renders_an_explicit_marker() {
        let questions = vec![Question {
            text: "q".to_owned(),
            options: vec![
                Choice {
                    label: "a".to_owned(),
                    description: None,
                },
                Choice {
                    label: "b".to_owned(),
                    description: None,
                },
            ],
        }];
        let rendered = render_answers(&questions, &[9]);
        assert!(rendered.contains("<invalid choice>"), "{rendered}");
    }

    #[test]
    fn several_questions_render_one_answer_each() {
        let questions = vec![
            Question {
                text: "first".to_owned(),
                options: vec![
                    Choice {
                        label: "a".to_owned(),
                        description: None,
                    },
                    Choice {
                        label: "b".to_owned(),
                        description: None,
                    },
                ],
            },
            Question {
                text: "second".to_owned(),
                options: vec![
                    Choice {
                        label: "c".to_owned(),
                        description: None,
                    },
                    Choice {
                        label: "d".to_owned(),
                        description: None,
                    },
                ],
            },
        ];
        let rendered = render_answers(&questions, &[0, 1]);
        assert!(rendered.contains("first\nAnswer: a"));
        assert!(rendered.contains("second\nAnswer: d"));
    }
}
