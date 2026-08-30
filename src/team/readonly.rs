//! Deterministic read-only Operate Team for the v0.1 vertical slice.
//!
//! This Team diagnoses from its Snapshot View alone: it verifies the View's content hash, reads
//! the sanitized document, and reports unhealthy resources, threatened critical dependencies, and
//! coverage gaps. It proposes no actions and touches no machine. The model-backed
//! [`super::HarnessOperateTeam`] implements the same `AgentTeamPort`; nothing in the Scheduler
//! changes between them.

use async_trait::async_trait;

use crate::domain::{Artifact, HealthState, Job, JobOutcome, JobResult, TeamCallback, TeamKind};
use crate::error::AgentResult;
use crate::ports::{AgentTeamPort, CancelSignal, TeamCallbackSink};
use crate::view::FileArtifactStore;

/// Read-only Operate Team that reasons deterministically over the Snapshot View.
pub struct ReadOnlyOperateTeam {
    artifacts: FileArtifactStore,
}

impl ReadOnlyOperateTeam {
    /// Creates a Team that reads View bodies through the given artifact store.
    pub fn new(artifacts: FileArtifactStore) -> Self {
        Self { artifacts }
    }
}

#[async_trait]
impl AgentTeamPort for ReadOnlyOperateTeam {
    /// This implementation handles Operate Jobs only.
    fn team_kind(&self) -> TeamKind {
        TeamKind::Operate
    }

    /// Verifies and reads the View, then reports one progress callback and one final diagnosis.
    async fn run_job(
        &self,
        job: &Job,
        snapshot_view: &Artifact,
        sink: &dyn TeamCallbackSink,
        cancel: CancelSignal,
    ) -> AgentResult<()> {
        sink.deliver(TeamCallback::new(
            job.issue_id,
            job.job_id,
            "Verifying and reading the Snapshot View",
        ))
        .await?;

        // Replayability in practice: the Team refuses a View whose bytes do not match the hash
        // bound into the Job.
        let bytes = self.artifacts.read_verified(snapshot_view)?;
        let view: serde_json::Value = serde_json::from_slice(&bytes)?;

        if cancel.is_cancelled() {
            let result = JobResult::new(JobOutcome::Failed, "Cancelled before diagnosis");
            return sink
                .deliver(
                    TeamCallback::new(job.issue_id, job.job_id, "Cancelled")
                        .with_final_result(result),
                )
                .await;
        }

        let empty = Vec::new();
        let resources = view["resources"].as_array().unwrap_or(&empty);
        let gaps = view["coverage_gaps"].as_array().unwrap_or(&empty);
        let dependencies = view["dependencies"].as_array().unwrap_or(&empty);

        let mut unhealthy = Vec::new();
        for resource in resources {
            let id = resource["resource_id"].as_str().unwrap_or("?");
            let health: HealthState =
                serde_json::from_value(resource["health"].clone()).unwrap_or(HealthState::Unknown);
            if !matches!(health, HealthState::Healthy) {
                unhealthy.push((id.to_string(), health));
            }
        }

        // A critical dependency pointing at an unhealthy resource threatens the dependent even
        // when the dependent itself still answers probes.
        let mut threatened = Vec::new();
        for dep in dependencies {
            let critical = dep["critical"].as_bool().unwrap_or(false);
            let to = dep["to_resource_id"].as_str().unwrap_or("?");
            let from = dep["from_resource_id"].as_str().unwrap_or("?");
            if critical && unhealthy.iter().any(|(id, _)| id == to) {
                threatened.push(format!("`{from}` critically depends on unhealthy `{to}`."));
            }
        }

        let mut lines = Vec::new();
        if unhealthy.is_empty() {
            lines.push("All probed resources report healthy.".to_string());
        } else {
            for (id, health) in &unhealthy {
                lines.push(format!("`{id}` is {health:?}."));
            }
        }
        lines.extend(threatened.iter().cloned());
        if !gaps.is_empty() {
            lines.push(format!(
                "{} coverage gap(s) limit this diagnosis; see the View for details.",
                gaps.len()
            ));
        }

        let mut result = JobResult::new(JobOutcome::DiagnosisOnly, lines.join(" "));
        result.artifact_ids.push(snapshot_view.artifact_id);
        for gap in gaps {
            if let Some(probe) = gap["probe_id"].as_str() {
                result.unresolved_questions.push(format!(
                    "No observation from probe `{probe}` on `{}`",
                    gap["resource_id"].as_str().unwrap_or("?")
                ));
            }
        }

        sink.deliver(
            TeamCallback::new(job.issue_id, job.job_id, "Diagnosis complete")
                .with_final_result(result),
        )
        .await
    }
}
