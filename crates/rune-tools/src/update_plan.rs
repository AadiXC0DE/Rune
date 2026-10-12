//! A bounded plan owned by the live conversation, without filesystem writes.

use std::collections::BTreeSet;

use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};

/// Largest plan accepted by one update.
pub const MAX_STEPS: usize = 64;
/// Largest step description in bytes.
pub const MAX_STEP_BYTES: usize = 1024;
/// Largest optional explanation in bytes.
pub const MAX_EXPLANATION_BYTES: usize = 4096;

/// Status of one plan step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Work has not started.
    Pending,
    /// Work is underway. At most one step may have this status.
    InProgress,
    /// Work is finished.
    Completed,
}

/// One distinct step in a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Description, with surrounding whitespace removed.
    pub step: String,
    /// Current status.
    pub status: Status,
}

/// A complete plan snapshot carried by status events and model results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanUpdate {
    /// Monotonically increasing revision, starting at one for the first change.
    pub version: u64,
    /// Optional reason for this update.
    pub explanation: Option<String>,
    /// Entire replacement plan, in execution order.
    pub plan: Vec<Step>,
}

/// Replaces the current in-memory plan supplied by the execution context.
#[derive(Debug, Default)]
pub struct UpdatePlan;

impl Tool for UpdatePlan {
    fn name(&self) -> &'static str {
        "update_plan"
    }

    fn description(&self) -> &'static str {
        "Replace the live plan with the complete ordered list of steps and their statuses: pending, in_progress, or completed. Steps must be distinct and at most one may be in_progress. Optionally explain the update. Returns the current version; identical retries keep that version. The plan lives in memory for this session and does not edit files."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "explanation": {"type": "string", "maxLength": MAX_EXPLANATION_BYTES},
                "plan": {
                    "type": "array", "maxItems": MAX_STEPS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string", "minLength": 1, "maxLength": MAX_STEP_BYTES},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["step", "status"], "additionalProperties": false
                    }
                }
            },
            "required": ["plan"], "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Plan
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let mut next = decode(arguments)?;
        let mut current = context
            .live_plan()
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the live plan lock was poisoned"))?;
        context.check_cancelled()?;
        let changed = next.plan != current.plan || next.explanation != current.explanation;
        next.version = if changed {
            current.version.checked_add(1).ok_or_else(|| {
                RuneError::new(ErrorCode::Internal, "the live plan version was exhausted")
            })?
        } else {
            current.version
        };
        let text = serde_json::to_string(&next)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?;
        let mut output = ToolOutput::success(text);
        if changed {
            *current = next.clone();
            output.plan_update = Some(next);
        }
        Ok(output)
    }
}

fn decode(arguments: &serde_json::Value) -> Result<PlanUpdate> {
    let raw = arguments
        .get("plan")
        .ok_or_else(|| RuneError::missing_field("update_plan arguments.plan"))?;
    let steps = raw
        .as_array()
        .ok_or_else(|| RuneError::invalid_field("plan", "expected an array"))?;
    if steps.len() > MAX_STEPS {
        return Err(RuneError::too_large("plan", steps.len(), MAX_STEPS));
    }
    let explanation = arguments
        .get("explanation")
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| RuneError::invalid_field("explanation", "expected a string"))?;
            if text.len() > MAX_EXPLANATION_BYTES {
                return Err(RuneError::too_large(
                    "explanation",
                    text.len(),
                    MAX_EXPLANATION_BYTES,
                ));
            }
            Ok(text.to_owned())
        })
        .transpose()?;
    let mut plan = Vec::with_capacity(steps.len());
    let mut descriptions = BTreeSet::new();
    let mut active = false;
    for (index, raw) in steps.iter().enumerate() {
        let field = format!("plan[{index}]");
        let description = raw
            .get("step")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                RuneError::invalid_field(format!("{field}.step"), "expected a nonempty string")
            })?;
        if description.len() > MAX_STEP_BYTES {
            return Err(RuneError::too_large(
                format!("{field}.step"),
                description.len(),
                MAX_STEP_BYTES,
            ));
        }
        let description = description.trim();
        if description.is_empty() || !descriptions.insert(description) {
            return Err(RuneError::invalid_field(
                format!("{field}.step"),
                "steps must be nonempty and distinct after trimming whitespace",
            ));
        }
        let status = match raw.get("status").and_then(serde_json::Value::as_str) {
            Some("pending") => Status::Pending,
            Some("in_progress") if !active => {
                active = true;
                Status::InProgress
            }
            Some("in_progress") => {
                return Err(RuneError::invalid_field(
                    format!("{field}.status"),
                    "at most one step may be in_progress",
                ));
            }
            Some("completed") => Status::Completed,
            _ => {
                return Err(RuneError::invalid_field(
                    format!("{field}.status"),
                    "expected pending, in_progress, or completed",
                ));
            }
        };
        plan.push(Step {
            step: description.to_owned(),
            status,
        });
    }
    Ok(PlanUpdate {
        version: 0,
        explanation,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn execution_context() -> ExecutionContext {
        // The tool must work even when there is no filesystem workspace.
        ExecutionContext::new(camino::Utf8PathBuf::from("/absent-plan-workspace"))
    }

    fn initial() -> serde_json::Value {
        json!({"plan": [
            {"step": "Inspect", "status": "in_progress"},
            {"step": "Implement", "status": "pending"},
            {"step": "Verify", "status": "pending"}
        ]})
    }

    #[test]
    fn updates_replace_steps_and_identical_retries_keep_the_version() {
        let tool = UpdatePlan;
        let context = execution_context();
        assert!(!tool.is_read_only(), "plan updates must execute in order");
        assert_eq!(tool.permission_target(&initial()), None);
        let first = tool.call(&initial(), &context).expect("first update");
        assert_eq!(first.plan_update.as_ref().expect("event").version, 1);
        let mut next = initial();
        next["plan"][0]["status"] = json!("completed");
        next["plan"][1]["status"] = json!("in_progress");
        next["explanation"] = json!("Inspection finished");
        let second = tool.call(&next, &context).expect("second update");
        let snapshot = second.plan_update.expect("event");
        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.plan.len(), 3);
        assert_eq!(snapshot.plan[0].status, Status::Completed);
        assert_eq!(snapshot.plan[1].status, Status::InProgress);
        assert_eq!(snapshot.explanation.as_deref(), Some("Inspection finished"));
        assert_eq!(
            serde_json::from_str::<PlanUpdate>(&second.text).expect("result"),
            snapshot
        );
        next["plan"][0]["step"] = json!(" Inspect ");
        let replay = tool.call(&next, &context).expect("retry");
        assert!(replay.plan_update.is_none());
        assert_eq!(
            serde_json::from_str::<PlanUpdate>(&replay.text)
                .expect("result")
                .version,
            2
        );
        let replacement = tool
            .call(
                &json!({"plan": [{"step": "Finish", "status": "completed"}]}),
                &context,
            )
            .expect("replacement");
        assert_eq!(replacement.plan_update.expect("event").plan.len(), 1);
        let clear = tool.call(&json!({"plan": []}), &context).expect("clear");
        assert_eq!(clear.plan_update.expect("event").version, 4);
        assert!(
            tool.call(&json!({"plan": []}), &context)
                .expect("retry clear")
                .plan_update
                .is_none()
        );
        let separate = UpdatePlan
            .call(&initial(), &execution_context())
            .expect("separate session");
        assert_eq!(separate.plan_update.expect("event").version, 1);
    }

    #[test]
    fn invalid_updates_leave_the_current_plan_and_version_unchanged() {
        let tool = UpdatePlan;
        let context = execution_context();
        let first = tool.call(&initial(), &context).expect("initial");
        for bad in [
            json!({}),
            json!({"plan": null}),
            json!({"plan": {}}),
            json!({"plan": [null]}),
            json!({"plan": [{"step": "", "status": "pending"}]}),
            json!({"plan": [{"step": " \n ", "status": "pending"}]}),
            json!({"plan": [{"step": "Inspect", "status": "pending"}, {"step": " Inspect ", "status": "completed"}]}),
            json!({"plan": [{"step": "Inspect", "status": "in_progress"}, {"step": "Verify", "status": "in_progress"}]}),
            json!({"plan": [{"step": "Inspect", "status": "done"}]}),
            json!({"plan": [{"step": "Inspect"}]}),
            json!({"plan": [{"step": 1, "status": "pending"}]}),
            json!({"plan": [{"step": "Inspect", "status": "pending", "extra": true}]}),
            json!({"plan": [], "extra": true}),
            json!({"plan": [], "explanation": null}),
            json!({"plan": [], "explanation": "é".repeat(MAX_EXPLANATION_BYTES / 2 + 1)}),
            json!({"plan": [{"step": "é".repeat(MAX_STEP_BYTES / 2 + 1), "status": "pending"}]}),
            json!({"plan": vec![json!({"step": "Inspect", "status": "pending"}); MAX_STEPS + 1]}),
        ] {
            assert!(tool.call(&bad, &context).is_err(), "accepted {bad}");
            let replay = tool.call(&initial(), &context).expect("unchanged");
            assert_eq!(replay.text, first.text);
            assert!(replay.plan_update.is_none());
        }
    }

    #[test]
    fn cancellation_and_version_overflow_do_not_change_the_plan() {
        let tool = UpdatePlan;
        let context = execution_context();
        context.cancellation().cancel();
        assert_eq!(
            tool.call(&initial(), &context)
                .expect_err("cancelled")
                .code(),
            ErrorCode::Cancelled
        );
        assert_eq!(context.live_plan().lock().expect("lock").version, 0);
        let overflow = execution_context();
        overflow.live_plan().lock().expect("lock").version = u64::MAX;
        assert!(tool.call(&initial(), &overflow).is_err());
        assert!(overflow.live_plan().lock().expect("lock").plan.is_empty());
    }

    #[test]
    fn maximum_sized_plan_is_accepted_and_remains_bounded() {
        let tool = UpdatePlan;
        let context = execution_context();
        let plan: Vec<_> = (0..MAX_STEPS).map(|index| {
            let prefix = index.to_string();
            json!({"step": format!("{prefix}{}", "x".repeat(MAX_STEP_BYTES - prefix.len())), "status": "pending"})
        }).collect();
        let output = tool
            .call(
                &json!({"plan": plan, "explanation": "x".repeat(MAX_EXPLANATION_BYTES)}),
                &context,
            )
            .expect("maximum plan");
        assert_eq!(output.plan_update.expect("event").plan.len(), MAX_STEPS);
        assert!(output.text.len() < 128 * 1024);
    }
}
