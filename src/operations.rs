//! Activity and cancellation for an entire operator request, including work before a Job exists.
//!
//! The task-local context follows awaited calls through the scheduler and platform. Spawned
//! children explicitly clone its signal; unrelated requests never share cancellation state.

use crate::{
    domain::{ActionRunId, IssueId, JobId, NewEvent},
    error::{AgentError, AgentResult},
    ports::{CancelHandle, CancelSignal, StateStore, cancel_pair},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Current stage of an operator request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    /// Reading deployment state.
    Capture,
    /// Waiting for the model or a retry.
    Model,
    /// Running a read-only inspection.
    Inspection,
    /// Executing an approved command, including waiting for its target lock.
    Action,
    /// Checking the effect of a command that already finished.
    Verification,
}

/// One live activity shown by all operator interfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveOperation {
    /// Stable cancellation ID, available even before an Issue or Job exists.
    pub operation_id: Uuid,
    /// Entry point that owns the operation.
    pub kind: String,
    /// Current work stage.
    pub phase: OperationPhase,
    /// When this request started; clients derive continuously updating elapsed time.
    pub started_at: DateTime<Utc>,
    /// Issue, once created.
    pub issue_id: Option<IssueId>,
    /// Current or originating Job, once known.
    pub job_id: Option<JobId>,
    /// Action currently being executed or verified.
    pub action_run_id: Option<ActionRunId>,
    /// Whether a stop has been requested and cleanup is in progress.
    pub cancel_requested: bool,
}

struct Entry {
    state: ActiveOperation,
    cancel: CancelHandle,
}
#[derive(Default)]
struct Registry {
    active: BTreeMap<Uuid, Entry>,
    finished: VecDeque<(Uuid, bool)>,
}

/// Shared registry owned by one controller.
#[derive(Clone)]
pub struct Operations {
    registry: Arc<Mutex<Registry>>,
    store: Arc<dyn StateStore>,
}

#[derive(Clone)]
struct Context {
    id: Uuid,
    operations: Operations,
    signal: CancelSignal,
}
tokio::task_local! { static CURRENT: Context; }
tokio::task_local! { static CHILD_CANCEL: CancelSignal; }

struct Guard {
    id: Uuid,
    operations: Operations,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut registry = self.operations.registry.lock().unwrap();
        if let Some(entry) = registry.active.remove(&self.id) {
            registry
                .finished
                .push_back((self.id, entry.state.cancel_requested));
            while registry.finished.len() > 256 {
                registry.finished.pop_front();
            }
        }
    }
}

impl Operations {
    /// Attaches activity events to the same persistent event stream as task progress.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry::default())),
            store,
        }
    }

    /// Snapshot of live operations; no lock is held while a caller renders it.
    pub fn active(&self) -> Vec<ActiveOperation> {
        self.registry
            .lock()
            .unwrap()
            .active
            .values()
            .map(|e| e.state.clone())
            .collect()
    }

    async fn event(&self, kind: &str, state: &ActiveOperation, detail: &str) -> AgentResult<()> {
        let mut event = NewEvent::new(
            "runtime",
            kind,
            format!("{}: {:?} — {detail}", state.kind, state.phase),
        )
        .with_payload(serde_json::to_value(state)?);
        if let Some(id) = state.issue_id {
            event = event.with_issue(id);
        }
        if let Some(id) = state.job_id {
            event = event.with_job(id);
        }
        if let Some(id) = state.action_run_id {
            event = event.with_action(id);
        }
        self.store.append_event(event).await?;
        Ok(())
    }

    /// Runs an entry point under one cancellation scope. Nested runner calls retain that scope.
    pub async fn run<T>(
        &self,
        kind: &str,
        work: impl Future<Output = AgentResult<T>>,
    ) -> AgentResult<T> {
        self.run_with_id(kind, Uuid::now_v7(), work).await
    }

    /// Runs a request with the caller's correlation ID (for progress before Job admission).
    pub async fn run_with_id<T>(
        &self,
        kind: &str,
        id: Uuid,
        work: impl Future<Output = AgentResult<T>>,
    ) -> AgentResult<T> {
        if CURRENT
            .try_with(|ctx| Arc::ptr_eq(&ctx.operations.registry, &self.registry))
            .unwrap_or(false)
        {
            check()?;
            return work.await;
        }
        let (cancel, signal) = cancel_pair();
        let state = ActiveOperation {
            operation_id: id,
            kind: kind.into(),
            phase: OperationPhase::Capture,
            started_at: Utc::now(),
            issue_id: None,
            job_id: None,
            action_run_id: None,
            cancel_requested: false,
        };
        {
            let mut registry = self.registry.lock().unwrap();
            if registry.active.contains_key(&id) {
                return Err(AgentError::Duplicate {
                    entity: "Operation",
                    id: id.to_string(),
                });
            }
            registry.active.insert(
                id,
                Entry {
                    state: state.clone(),
                    cancel,
                },
            );
        }
        let guard = Guard {
            id,
            operations: self.clone(),
        };
        self.event("operation.started", &state, "started").await?;
        let result = CURRENT
            .scope(
                Context {
                    id,
                    operations: self.clone(),
                    signal,
                },
                work,
            )
            .await;
        let state = self
            .registry
            .lock()
            .unwrap()
            .active
            .get(&id)
            .map(|e| e.state.clone())
            .unwrap_or(state);
        drop(guard);
        self.event(
            "operation.finished",
            &state,
            if state.cancel_requested {
                "cancelled; recorded effects may require verification"
            } else if result.is_err() {
                "failed"
            } else {
                "finished"
            },
        )
        .await?;
        result
    }

    /// Requests a stop once; repeated requests during cleanup or after a cancelled finish succeed.
    pub async fn cancel(&self, id: Uuid, by: &str) -> AgentResult<bool> {
        let state = {
            let mut registry = self.registry.lock().unwrap();
            let Some(entry) = registry.active.get_mut(&id) else {
                return Ok(registry
                    .finished
                    .iter()
                    .any(|(known, cancelled)| *known == id && *cancelled));
            };
            if entry.state.cancel_requested {
                return Ok(true);
            }
            entry.state.cancel_requested = true;
            entry.cancel.cancel();
            entry.state.clone()
        };
        self.event("operation.cancel_requested", &state, by).await?;
        Ok(true)
    }

    /// Cancels activities associated with a Job for the legacy Job cancellation API.
    pub async fn cancel_job(&self, job_id: JobId, by: &str) -> AgentResult<bool> {
        let ids: Vec<_> = self
            .active()
            .into_iter()
            .filter(|op| op.job_id == Some(job_id))
            .map(|op| op.operation_id)
            .collect();
        for id in &ids {
            self.cancel(*id, by).await?;
        }
        Ok(!ids.is_empty())
    }

    /// Cancels every active request, including requests that have not created a Job yet.
    pub async fn cancel_all(&self, by: &str) -> AgentResult<usize> {
        let active = self.active();
        for op in &active {
            self.cancel(op.operation_id, by).await?;
        }
        Ok(active.len())
    }
}

/// Clones the current signal for a spawned child that does not inherit task locals.
pub fn signal() -> Option<CancelSignal> {
    CURRENT.try_with(|ctx| ctx.signal.clone()).ok()
}

/// ID of the current activity, used to avoid cancelling a response that already finished.
pub fn current_id() -> Option<Uuid> {
    CURRENT.try_with(|ctx| ctx.id).ok()
}

/// Updates the model phase at the actual step boundary, before asynchronous display messages
/// can lag behind a subsequent inspection. The Team callback supplies the corresponding event.
pub fn model_started() {
    let _ = CURRENT.try_with(|ctx| {
        if let Some(entry) = ctx
            .operations
            .registry
            .lock()
            .unwrap()
            .active
            .get_mut(&ctx.id)
        {
            entry.state.phase = OperationPhase::Model;
            entry.state.action_run_id = None;
        }
    });
}

/// Refuses subsequent work after cancellation, without interrupting cleanup or persistence.
pub fn check() -> AgentResult<()> {
    if signal().is_some_and(|signal| signal.is_cancelled())
        || CHILD_CANCEL
            .try_with(CancelSignal::is_cancelled)
            .unwrap_or(false)
    {
        Err(AgentError::Cancelled)
    } else {
        Ok(())
    }
}

/// Waits for the current operation's cancellation, or forever outside an operation scope.
pub async fn cancelled() {
    async fn wait(signal: Option<CancelSignal>) {
        if let Some(mut signal) = signal {
            signal.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
    tokio::select! { () = wait(signal()) => {}, () = wait(CHILD_CANCEL.try_with(Clone::clone).ok()) => {} }
}

/// Supplies a tool's own cancellation/timeout signal while retaining its parent activity.
pub async fn with_signal<T>(signal: CancelSignal, work: impl Future<Output = T>) -> T {
    CHILD_CANCEL.scope(signal, work).await
}

/// Races a disposable read or lock acquisition, never a command owner that must save its output.
pub async fn cancellable<T>(work: impl Future<Output = AgentResult<T>>) -> AgentResult<T> {
    check()?;
    tokio::select! { biased; () = cancelled() => Err(AgentError::Cancelled), result = work => result }
}

/// Records a phase and updates associations as they become known.
pub async fn phase(
    phase: OperationPhase,
    issue: Option<IssueId>,
    job: Option<JobId>,
    action: Option<ActionRunId>,
) -> AgentResult<()> {
    let Ok(ctx) = CURRENT.try_with(Clone::clone) else {
        return Ok(());
    };
    let state = {
        let mut registry = ctx.operations.registry.lock().unwrap();
        let Some(entry) = registry.active.get_mut(&ctx.id) else {
            return Ok(());
        };
        entry.state.phase = phase;
        if issue.is_some() {
            entry.state.issue_id = issue;
        }
        if job.is_some() {
            entry.state.job_id = job;
        }
        entry.state.action_run_id = action;
        entry.state.clone()
    };
    ctx.operations
        .event("operation.progress", &state, "stage changed")
        .await
}

/// Current phase, when a caller must retain verification rather than relabel it as capture.
pub fn current_phase() -> Option<OperationPhase> {
    CURRENT
        .try_with(|ctx| {
            ctx.operations
                .registry
                .lock()
                .unwrap()
                .active
                .get(&ctx.id)
                .map(|e| e.state.phase)
        })
        .ok()
        .flatten()
}
