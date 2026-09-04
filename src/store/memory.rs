//! In-process Store for initial reading, tests, and dependency wiring.
//!
//! This implementation uses one Tokio `RwLock` for single-process consistency. It does not persist
//! across processes or simulate SQLite transactions; a future database implementation should retain
//! the same `StateStore` contract.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::domain::{
    ActionRun, ActionRunId, Artifact, ArtifactId, EventRecord, Issue, IssueId, Job, JobId,
    NewEvent, Snapshot, SnapshotId,
};
use crate::error::{AgentError, AgentResult};
use crate::ports::StateStore;

/// Complete state protected by the in-memory Store lock.
#[derive(Debug)]
struct StoreState {
    snapshots: HashMap<SnapshotId, Snapshot>,
    issues: HashMap<IssueId, Issue>,
    jobs: HashMap<JobId, Job>,
    action_runs: HashMap<ActionRunId, ActionRun>,
    artifacts: HashMap<ArtifactId, Artifact>,
    events: Vec<EventRecord>,
    next_event_sequence: u64,
}

impl Default for StoreState {
    /// Creates empty state and begins assigning EventLog sequence numbers at one.
    fn default() -> Self {
        Self {
            snapshots: HashMap::new(),
            issues: HashMap::new(),
            jobs: HashMap::new(),
            action_runs: HashMap::new(),
            artifacts: HashMap::new(),
            events: Vec::new(),
            next_event_sequence: 1,
        }
    }
}

/// `StateStore` implementation backed by in-process HashMaps and an Event Vec.
#[derive(Debug, Default)]
pub struct InMemoryStateStore {
    inner: RwLock<StoreState>,
}

impl InMemoryStateStore {
    /// Creates a Store with no Snapshots, Issues, Jobs, ActionRuns, Artifacts, or Events.
    ///
    /// This Store is suitable only for the initial version and tests; process exit loses all data.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateStore for InMemoryStateStore {
    /// Inserts a Snapshot as an immutable object; a duplicate ID returns Duplicate.
    async fn insert_snapshot(&self, snapshot: Snapshot) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if state.snapshots.contains_key(&snapshot.snapshot_id) {
            return Err(duplicate("Snapshot", snapshot.snapshot_id));
        }
        state.snapshots.insert(snapshot.snapshot_id, snapshot);
        Ok(())
    }

    /// Returns a clone of the Snapshot so callers cannot modify the canonical Store value.
    async fn get_snapshot(&self, snapshot_id: SnapshotId) -> AgentResult<Snapshot> {
        self.inner
            .read()
            .await
            .snapshots
            .get(&snapshot_id)
            .cloned()
            .ok_or_else(|| not_found("Snapshot", snapshot_id))
    }

    /// Inserts a new Issue; a duplicate ID returns Duplicate.
    async fn insert_issue(&self, issue: Issue) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if state.issues.contains_key(&issue.issue_id) {
            return Err(duplicate("Issue", issue.issue_id));
        }
        state.issues.insert(issue.issue_id, issue);
        Ok(())
    }

    /// Replaces existing Issue materialized state; absence returns NotFound.
    async fn update_issue(&self, issue: Issue) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if !state.issues.contains_key(&issue.issue_id) {
            return Err(not_found("Issue", issue.issue_id));
        }
        state.issues.insert(issue.issue_id, issue);
        Ok(())
    }

    /// Returns a clone of the specified Issue.
    async fn get_issue(&self, issue_id: IssueId) -> AgentResult<Issue> {
        self.inner
            .read()
            .await
            .issues
            .get(&issue_id)
            .cloned()
            .ok_or_else(|| not_found("Issue", issue_id))
    }

    /// Returns all non-terminal Issues in stable creation-time and ID order.
    async fn list_unfinished_issues(&self) -> AgentResult<Vec<Issue>> {
        let mut issues: Vec<_> = self
            .inner
            .read()
            .await
            .issues
            .values()
            .filter(|issue| !issue.status.is_terminal())
            .cloned()
            .collect();
        issues.sort_by_key(|issue| (issue.created_at, issue.issue_id));
        Ok(issues)
    }

    /// Inserts a new Job; a duplicate ID returns Duplicate.
    async fn insert_job(&self, job: Job) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if state.jobs.contains_key(&job.job_id) {
            return Err(duplicate("Job", job.job_id));
        }
        state.jobs.insert(job.job_id, job);
        Ok(())
    }

    /// Replaces existing Job materialized state; absence returns NotFound.
    async fn update_job(&self, job: Job) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if !state.jobs.contains_key(&job.job_id) {
            return Err(not_found("Job", job.job_id));
        }
        state.jobs.insert(job.job_id, job);
        Ok(())
    }

    /// Returns a clone of the specified Job.
    async fn get_job(&self, job_id: JobId) -> AgentResult<Job> {
        self.inner
            .read()
            .await
            .jobs
            .get(&job_id)
            .cloned()
            .ok_or_else(|| not_found("Job", job_id))
    }

    /// Returns all non-terminal Jobs in stable creation-time and ID order.
    async fn list_unfinished_jobs(&self) -> AgentResult<Vec<Job>> {
        let mut jobs: Vec<_> = self
            .inner
            .read()
            .await
            .jobs
            .values()
            .filter(|job| !job.status.is_terminal())
            .cloned()
            .collect();
        jobs.sort_by_key(|job| (job.created_at, job.job_id));
        Ok(jobs)
    }

    /// Inserts a new ActionRun; a duplicate ID returns Duplicate.
    async fn insert_action_run(&self, action: ActionRun) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if state.action_runs.contains_key(&action.action_run_id) {
            return Err(duplicate("ActionRun", action.action_run_id));
        }
        state.action_runs.insert(action.action_run_id, action);
        Ok(())
    }

    /// Replaces existing ActionRun materialized state; absence returns NotFound.
    async fn update_action_run(&self, action: ActionRun) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if !state.action_runs.contains_key(&action.action_run_id) {
            return Err(not_found("ActionRun", action.action_run_id));
        }
        state.action_runs.insert(action.action_run_id, action);
        Ok(())
    }

    /// Returns a clone of the specified ActionRun.
    async fn get_action_run(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.inner
            .read()
            .await
            .action_runs
            .get(&action_run_id)
            .cloned()
            .ok_or_else(|| not_found("ActionRun", action_run_id))
    }

    /// Returns all non-terminal ActionRuns in stable creation-time and ID order.
    async fn list_unfinished_action_runs(&self) -> AgentResult<Vec<ActionRun>> {
        let mut actions: Vec<_> = self
            .inner
            .read()
            .await
            .action_runs
            .values()
            .filter(|action| !action.status.is_terminal())
            .cloned()
            .collect();
        actions.sort_by_key(|action| (action.created_at, action.action_run_id));
        Ok(actions)
    }

    /// Returns every ActionRun in stable creation-time and ID order.
    async fn list_action_runs(&self) -> AgentResult<Vec<ActionRun>> {
        let mut actions: Vec<_> = self
            .inner
            .read()
            .await
            .action_runs
            .values()
            .cloned()
            .collect();
        actions.sort_by_key(|action| (action.created_at, action.action_run_id));
        Ok(actions)
    }

    /// Returns every Issue in stable creation-time and ID order.
    async fn list_issues(&self) -> AgentResult<Vec<Issue>> {
        let mut items: Vec<_> = self.inner.read().await.issues.values().cloned().collect();
        items.sort_by_key(|item| (item.created_at, item.issue_id));
        Ok(items)
    }

    /// Returns every Job in stable creation-time and ID order.
    async fn list_jobs(&self) -> AgentResult<Vec<Job>> {
        let mut items: Vec<_> = self.inner.read().await.jobs.values().cloned().collect();
        items.sort_by_key(|item| (item.created_at, item.job_id));
        Ok(items)
    }

    /// Returns every Snapshot in stable creation-time and ID order.
    async fn list_snapshots(&self) -> AgentResult<Vec<Snapshot>> {
        let mut items: Vec<_> = self
            .inner
            .read()
            .await
            .snapshots
            .values()
            .cloned()
            .collect();
        items.sort_by_key(|item| (item.created_at, item.snapshot_id));
        Ok(items)
    }

    /// Inserts immutable Artifact metadata; a duplicate ID returns Duplicate.
    async fn insert_artifact(&self, artifact: Artifact) -> AgentResult<()> {
        let mut state = self.inner.write().await;
        if state.artifacts.contains_key(&artifact.artifact_id) {
            return Err(duplicate("Artifact", artifact.artifact_id));
        }
        state.artifacts.insert(artifact.artifact_id, artifact);
        Ok(())
    }

    /// Returns a clone of the specified Artifact metadata.
    async fn get_artifact(&self, artifact_id: ArtifactId) -> AgentResult<Artifact> {
        self.inner
            .read()
            .await
            .artifacts
            .get(&artifact_id)
            .cloned()
            .ok_or_else(|| not_found("Artifact", artifact_id))
    }

    /// Assigns a sequence and appends the event under one write lock, preserving unique stable order
    /// under concurrent calls.
    async fn append_event(&self, event: NewEvent) -> AgentResult<EventRecord> {
        let mut state = self.inner.write().await;
        let sequence = state.next_event_sequence;
        state.next_event_sequence += 1;
        let record = EventRecord::from_new(sequence, event);
        state.events.push(record.clone());
        Ok(record)
    }

    /// Returns a clone of the complete EventLog ordered by sequence.
    async fn list_events(&self) -> AgentResult<Vec<EventRecord>> {
        Ok(self.inner.read().await.events.clone())
    }
}

/// Creates a Duplicate error containing the entity kind and ID.
fn duplicate(entity: &'static str, id: impl ToString) -> AgentError {
    AgentError::Duplicate {
        entity,
        id: id.to_string(),
    }
}

/// Creates a NotFound error containing the entity kind and ID.
fn not_found(entity: &'static str, id: impl ToString) -> AgentError {
    AgentError::NotFound {
        entity,
        id: id.to_string(),
    }
}
