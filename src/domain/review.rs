//! Human decisions on Team output: denials with their reasons, inbox reviews, and the feedback
//! that flows back upstream into the next Job.
//!
//! These records exist so that a refusal is never just an event in the log. A denied or failed
//! item stays visible in the inbox until a human reviews it, and when the human sends it back
//! upstream the reason and their comments become input to the next processing pass rather than
//! history to be scrolled past.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{ActionRunId, JobId, ResourceId};
use crate::tr;

/// Who refused an ActionRun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialSource {
    /// The authority matrix or the Runbook Registry refused the proposal; no human was asked.
    Policy,
    /// A human rejected the proposal from the Permission Request inbox.
    Human,
}

/// Why an ActionRun will not execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denial {
    /// Rule or human.
    pub source: DenialSource,
    /// The matrix rationale, or the fixed description of a human rejection.
    pub reason: String,
    /// Free-text comment from the human, when one was given.
    pub comment: Option<String>,
    /// Identity of the deciding human, for human denials.
    pub decided_by: Option<String>,
    /// When the denial was recorded.
    pub decided_at: DateTime<Utc>,
}

impl Denial {
    /// A denial by rule, carrying the rule's rationale verbatim.
    pub fn by_policy(reason: impl Into<String>) -> Self {
        Self {
            source: DenialSource::Policy,
            reason: reason.into(),
            comment: None,
            decided_by: None,
            decided_at: Utc::now(),
        }
    }

    /// A denial by a human, with an optional comment explaining it.
    pub fn by_human(decided_by: impl Into<String>, comment: Option<String>) -> Self {
        Self {
            source: DenialSource::Human,
            reason: "rejected by a human from the Permission Request inbox".to_string(),
            comment: comment.filter(|text| !text.trim().is_empty()),
            decided_by: Some(decided_by.into()),
            decided_at: Utc::now(),
        }
    }
}

/// What a human decided after reviewing a denied or failed item in the inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ReviewDecision {
    /// The human took note; no further automatic work follows from this item.
    Acknowledged,
    /// The human sent the item back upstream; the named Job carries the feedback.
    SentUpstream {
        /// The revising Job created for the feedback.
        job_id: JobId,
    },
}

/// A human's review of one inbox item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanReview {
    /// Identity of the reviewer as presented to the API or CLI.
    pub reviewer: String,
    /// What the reviewer decided.
    pub decision: ReviewDecision,
    /// Free-text comment, when one was given.
    pub comment: Option<String>,
    /// When the review was recorded.
    pub reviewed_at: DateTime<Utc>,
}

impl HumanReview {
    /// Creates a review record now.
    pub fn new(
        reviewer: impl Into<String>,
        decision: ReviewDecision,
        comment: Option<String>,
    ) -> Self {
        Self {
            reviewer: reviewer.into(),
            decision,
            comment: comment.filter(|text| !text.trim().is_empty()),
            reviewed_at: Utc::now(),
        }
    }
}

/// What a piece of upstream feedback is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FeedbackOrigin {
    /// A proposed action was denied by rule or by a human.
    DeniedAction {
        /// The denied ActionRun.
        action_run_id: ActionRunId,
        /// Runbook that was proposed.
        runbook_id: String,
        /// Targets that were proposed.
        target_ids: Vec<ResourceId>,
        /// The denial, including its reason and any comment.
        denial: Denial,
    },
    /// An action executed but failed, or its expected effect did not appear.
    FailedAction {
        /// The failed ActionRun.
        action_run_id: ActionRunId,
        /// Runbook that ran.
        runbook_id: String,
        /// Targets it ran against.
        target_ids: Vec<ResourceId>,
        /// The Platform or verification summary.
        summary: String,
        /// Sanitized execution evidence (exit codes, the tail of stderr and stdout), when the
        /// Platform recorded output. Machine text: the View fences it as untrusted data.
        #[serde(default)]
        evidence: Option<String>,
    },
    /// A Job failed before producing a usable result.
    FailedJob {
        /// The failed Job.
        job_id: JobId,
        /// The Team's failure summary.
        summary: String,
    },
    /// A Job asked for more observations when no automatic pass was left to collect them.
    StalledJob {
        /// The stalled Job.
        job_id: JobId,
        /// The Team's summary of what it still needed.
        summary: String,
        /// Probe IDs it asked for.
        requested_probe_ids: Vec<String>,
    },
}

/// Human-confirmed feedback that participates in the next processing pass.
///
/// The Scheduler copies every earlier feedback record into a revising Job, so a Team always sees
/// the whole conversation with the humans about this Issue, not only the latest turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanFeedback {
    /// Feedback ID.
    pub feedback_id: Uuid,
    /// What the feedback is about.
    pub origin: FeedbackOrigin,
    /// Identity of the human who sent it upstream.
    pub reviewer: String,
    /// The human's comment, when one was given.
    pub comment: Option<String>,
    /// When the feedback was recorded.
    pub recorded_at: DateTime<Utc>,
}

impl HumanFeedback {
    /// Creates a feedback record now.
    pub fn new(
        origin: FeedbackOrigin,
        reviewer: impl Into<String>,
        comment: Option<String>,
    ) -> Self {
        Self {
            feedback_id: Uuid::now_v7(),
            origin,
            reviewer: reviewer.into(),
            comment: comment.filter(|text| !text.trim().is_empty()),
            recorded_at: Utc::now(),
        }
    }

    /// Renders the feedback as the sentences a Team should read before its next pass.
    pub fn describe(&self) -> String {
        let mut text = match &self.origin {
            FeedbackOrigin::DeniedAction {
                runbook_id,
                target_ids,
                denial,
                ..
            } => {
                let by = match denial.source {
                    DenialSource::Policy => tr!("by rule", "按规则"),
                    DenialSource::Human => tr!("by a human", "由人工"),
                };
                let mut line = tr!(
                    format!(
                        "The proposed action `{runbook_id}` on {} was denied ({by}): {}.",
                        target_ids.join(", "),
                        denial.reason
                    ),
                    format!(
                        "针对 {} 提议的操作 `{runbook_id}` 已被拒绝（{by}）：{}。",
                        target_ids.join(", "),
                        denial.reason
                    )
                );
                if let Some(comment) = &denial.comment {
                    line.push_str(&tr!(
                        format!(" The human who denied it said: {comment}"),
                        format!(" 拒绝者的说明：{comment}")
                    ));
                }
                line
            }
            FeedbackOrigin::FailedAction {
                runbook_id,
                target_ids,
                summary,
                ..
            } => tr!(
                format!(
                    "The action `{runbook_id}` on {} ran but did not succeed: {summary}.",
                    target_ids.join(", ")
                ),
                format!(
                    "针对 {} 的操作 `{runbook_id}` 已执行但未成功：{summary}。",
                    target_ids.join(", ")
                )
            ),
            FeedbackOrigin::FailedJob { summary, .. } => tr!(
                format!("The previous Job failed: {summary}."),
                format!("上一轮任务失败：{summary}。")
            ),
            FeedbackOrigin::StalledJob {
                summary,
                requested_probe_ids,
                ..
            } => tr!(
                format!(
                    "The previous Job needed more observations ({}) but no automatic pass was \
                     left: {summary}.",
                    requested_probe_ids.join(", ")
                ),
                format!(
                    "上一轮任务需要更多观测（{}），但已没有自动轮次可用：{summary}。",
                    requested_probe_ids.join(", ")
                )
            ),
        };
        if let Some(comment) = &self.comment {
            text.push_str(&tr!(
                format!(" Reviewer {} added: {comment}", self.reviewer),
                format!(" 审核人 {} 补充：{comment}", self.reviewer)
            ));
        } else {
            text.push_str(&tr!(
                format!(
                    " Reviewer {} sent this back for another pass.",
                    self.reviewer
                ),
                format!(" 审核人 {} 将其送回以进行新一轮处理。", self.reviewer)
            ));
        }
        text
    }
}
