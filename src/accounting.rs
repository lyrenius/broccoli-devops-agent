//! Durable per-request model accounting, independent of Job completion and transcript archiving.

use crate::{
    domain::{IssueId, JobId, ModelUsage, NewEvent},
    operations::{OperationPhase, Operations},
    ports::StateStore,
    scheduler::TopScheduler,
    settings::SharedSettings,
    usage::{REQUEST_FINISHED, REQUEST_STARTED, RequestUsage, UsageTotals},
};
use async_trait::async_trait;
use broccoli_agent_harness::{
    HarnessError, HarnessResult, RequestObserver, RequestRecord, RequestStatus,
};
use std::sync::Arc;
use uuid::Uuid;

/// An awaited ledger writer for one Job or standalone model diagnostic.
pub struct RequestAccounting {
    store: Arc<dyn StateStore>,
    settings: SharedSettings,
    model: String,
    namespace: Uuid,
    job_id: Option<JobId>,
    issue_id: Option<IssueId>,
    control: Option<(Arc<TopScheduler>, Operations)>,
}

impl RequestAccounting {
    /// Namespaces request IDs by Job, or by a unique standalone diagnostic run.
    pub fn new(
        store: Arc<dyn StateStore>,
        settings: SharedSettings,
        model: String,
        namespace: Uuid,
        job_id: Option<JobId>,
        issue_id: Option<IssueId>,
    ) -> Self {
        Self {
            store,
            settings,
            model,
            namespace,
            job_id,
            issue_id,
            control: None,
        }
    }

    /// Adds deployment-wide budget enforcement to a controller-backed run.
    pub fn with_control(mut self, scheduler: Arc<TopScheduler>, operations: Operations) -> Self {
        self.control = Some((scheduler, operations));
        self
    }

    async fn budget_reason(&self) -> HarnessResult<Option<String>> {
        if !self.settings.read(|settings| settings.budget.is_set()) {
            return Ok(None);
        }
        let archived: std::collections::HashSet<_> = self
            .store
            .list_issues()
            .await
            .map_err(accounting_error)?
            .into_iter()
            .filter(|issue| issue.is_archived())
            .map(|issue| issue.issue_id)
            .collect();
        let events: Vec<_> = self
            .store
            .list_events()
            .await
            .map_err(accounting_error)?
            .into_iter()
            .filter(|event| event.issue_id.is_none_or(|id| !archived.contains(&id)))
            .collect();
        let settings = self.settings.current();
        let totals = UsageTotals::from_events(&events, settings.pricing.as_ref(), &settings.budget);
        let reason = settings.budget.exceeded_by(&totals);
        if let Some(reason) = &reason
            && let Some((scheduler, operations)) = &self.control
        {
            if scheduler.mode().await != crate::scheduler::SchedulerMode::FullyFrozen {
                self.store.append_event(NewEvent::new("top-scheduler", "scheduler.budget_exhausted",
                        format!("Dispatch is frozen because {reason}. Raise [budget], then resume from a console."))).await.map_err(accounting_error)?;
                scheduler.freeze_all().await.map_err(accounting_error)?;
            }
            for operation in operations.active().into_iter().filter(|op| {
                op.phase == OperationPhase::Model
                    && Some(op.operation_id) != crate::operations::current_id()
                    && totals.calls.iter().any(|call| {
                        call.request.job_id == op.job_id && call.request.status == "started"
                    })
            }) {
                operations
                    .cancel(operation.operation_id, "model spending budget")
                    .await
                    .map_err(accounting_error)?;
            }
        }
        Ok(reason)
    }
}

fn accounting_error(error: impl std::fmt::Display) -> HarnessError {
    HarnessError::Model(format!("request accounting: {error}"))
}

#[async_trait]
impl RequestObserver for RequestAccounting {
    async fn before_request(&self) -> HarnessResult<()> {
        match self.budget_reason().await? {
            Some(reason) => Err(HarnessError::Model(reason)),
            None => Ok(()),
        }
    }

    async fn record(&self, record: &RequestRecord) -> HarnessResult<()> {
        let counts = record.usage.map(|usage| ModelUsage {
            model: self.model.clone(),
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            requests: usage.requests,
            requests_without_usage: usage.requests_without_usage,
        });
        let status = match record.status {
            RequestStatus::Started => "started",
            RequestStatus::Succeeded => "succeeded",
            RequestStatus::Failed => "failed",
            RequestStatus::Cancelled => "cancelled",
            RequestStatus::NotSent => "not_sent",
        };
        let request = RequestUsage {
            request_id: format!("{}:{}", self.namespace, record.request_id),
            namespace: self.namespace,
            model: self.model.clone(),
            issue_id: self.issue_id,
            job_id: self.job_id,
            turn: record.turn,
            started_at: record.started_at,
            finished_at: record.finished_at,
            status: status.into(),
            usage: counts,
            error: record.error.clone(),
        };
        let mut event = NewEvent::new(
            "model",
            if record.status == RequestStatus::Started {
                REQUEST_STARTED
            } else {
                REQUEST_FINISHED
            },
            format!("request {} {status}", request.request_id),
        )
        .with_payload(serde_json::to_value(&request).map_err(accounting_error)?);
        if let Some(id) = self.job_id {
            event = event.with_job(id);
        }
        if let Some(id) = self.issue_id {
            event = event.with_issue(id);
        }
        self.store
            .append_event(event)
            .await
            .map_err(accounting_error)?;
        if record.status != RequestStatus::Started {
            if record.status != RequestStatus::NotSent {
                let counts = request.counts();
                let pricing = self.settings.current().pricing;
                let cost = pricing
                    .as_ref()
                    .map(|p| format!("{:.6} {}", p.cost(&counts), p.currency))
                    .unwrap_or_else(|| "unpriced".into());
                eprintln!(
                    "  · model request {} · input {} / output {} · {}{}",
                    request.request_id,
                    counts.input_tokens,
                    counts.output_tokens,
                    cost,
                    if counts.is_complete() {
                        ""
                    } else {
                        " (known portion only; usage incomplete)"
                    }
                );
            }
            self.budget_reason().await?;
        }
        Ok(())
    }
}
