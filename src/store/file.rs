//! File-backed `StateStore` so control state survives a controller restart.
//!
//! This is the v0.1 persistence described in the architecture document: an append-only
//! `events.jsonl` plus one JSON document per Snapshot, Issue, Job, ActionRun, and Artifact record.
//! It trades throughput for auditability — every object is a plain file an operator can open — and
//! keeps the exact `StateStore` contract so a later SQLite implementation is a drop-in swap.
//! Writes are not transactional across objects; the Scheduler documents which pairs a database
//! implementation must commit atomically.

use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::RwLock;

use crate::domain::{
    ActionRun, ActionRunId, Artifact, ArtifactId, EventRecord, Issue, IssueId, Job, JobId,
    NewEvent, Snapshot, SnapshotId,
};
use crate::error::{AgentError, AgentResult};
use crate::ports::StateStore;

/// Mutable index over the on-disk state, mirrored in memory for fast reads.
#[derive(Debug, Default)]
struct FileIndex {
    snapshots: HashMap<SnapshotId, Snapshot>,
    issues: HashMap<IssueId, Issue>,
    jobs: HashMap<JobId, Job>,
    action_runs: HashMap<ActionRunId, ActionRun>,
    artifacts: HashMap<ArtifactId, Artifact>,
    events: Vec<EventRecord>,
    next_event_sequence: u64,
}

/// `StateStore` that persists every object under one data directory.
#[derive(Debug)]
pub struct FileStateStore {
    root: PathBuf,
    index: RwLock<FileIndex>,
}

/// Reads every `*.json` document in a directory into a map keyed by the given function.
fn load_dir<T, K>(dir: &Path, key: fn(&T) -> K) -> AgentResult<HashMap<K, T>>
where
    T: DeserializeOwned,
    K: std::hash::Hash + Eq,
{
    let mut map = HashMap::new();
    if !dir.exists() {
        return Ok(map);
    }
    let entries = std::fs::read_dir(dir).map_err(|source| AgentError::Io {
        context: format!("listing `{}`", dir.display()),
        source,
    })?;
    for entry in entries {
        let path = entry
            .map_err(|source| AgentError::Io {
                context: format!("listing `{}`", dir.display()),
                source,
            })?
            .path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|source| AgentError::Io {
            context: format!("reading `{}`", path.display()),
            source,
        })?;
        let value: T = serde_json::from_str(&text)?;
        map.insert(key(&value), value);
    }
    Ok(map)
}

impl FileStateStore {
    /// Opens (or initializes) a data directory and rebuilds the in-memory index from it.
    ///
    /// Recovery is exactly this constructor: after a controller restart, everything the previous
    /// process persisted is readable again, and event sequencing continues from the last line of
    /// `events.jsonl`.
    pub fn open(root: impl Into<PathBuf>) -> AgentResult<Self> {
        let root = root.into();
        for sub in ["snapshots", "issues", "jobs", "action_runs", "artifacts"] {
            std::fs::create_dir_all(root.join(sub)).map_err(|source| AgentError::Io {
                context: format!("creating `{}`", root.join(sub).display()),
                source,
            })?;
        }

        let mut events = Vec::new();
        let events_path = root.join("events.jsonl");
        if events_path.exists() {
            let file = std::fs::File::open(&events_path).map_err(|source| AgentError::Io {
                context: format!("reading `{}`", events_path.display()),
                source,
            })?;
            for line in std::io::BufReader::new(file).lines() {
                let line = line.map_err(|source| AgentError::Io {
                    context: format!("reading `{}`", events_path.display()),
                    source,
                })?;
                if line.trim().is_empty() {
                    continue;
                }
                events.push(serde_json::from_str::<EventRecord>(&line)?);
            }
        }
        let next_event_sequence = events.last().map_or(1, |event| event.sequence + 1);

        let index = FileIndex {
            snapshots: load_dir(&root.join("snapshots"), |s: &Snapshot| s.snapshot_id)?,
            issues: load_dir(&root.join("issues"), |i: &Issue| i.issue_id)?,
            jobs: load_dir(&root.join("jobs"), |j: &Job| j.job_id)?,
            action_runs: load_dir(&root.join("action_runs"), |a: &ActionRun| a.action_run_id)?,
            artifacts: load_dir(&root.join("artifacts"), |a: &Artifact| a.artifact_id)?,
            events,
            next_event_sequence,
        };
        Ok(Self {
            root,
            index: RwLock::new(index),
        })
    }

    /// Writes one object as a pretty JSON document under the given subdirectory.
    fn write_doc<T: Serialize>(&self, sub: &str, id: impl ToString, value: &T) -> AgentResult<()> {
        let path = self.root.join(sub).join(format!("{}.json", id.to_string()));
        let text = serde_json::to_string_pretty(value)?;
        std::fs::write(&path, text).map_err(|source| AgentError::Io {
            context: format!("writing `{}`", path.display()),
            source,
        })
    }

    /// Appends one event line to `events.jsonl` and flushes it.
    fn append_event_line(&self, record: &EventRecord) -> AgentResult<()> {
        let path = self.root.join("events.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| AgentError::Io {
                context: format!("opening `{}`", path.display()),
                source,
            })?;
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|source| AgentError::Io {
                context: format!("appending to `{}`", path.display()),
                source,
            })
    }
}

/// Builds the standard Duplicate error.
fn duplicate(entity: &'static str, id: impl ToString) -> AgentError {
    AgentError::Duplicate {
        entity,
        id: id.to_string(),
    }
}

/// Builds the standard NotFound error.
fn not_found(entity: &'static str, id: impl ToString) -> AgentError {
    AgentError::NotFound {
        entity,
        id: id.to_string(),
    }
}

#[async_trait]
impl StateStore for FileStateStore {
    /// Persists an immutable Snapshot document; a duplicate ID returns Duplicate.
    async fn insert_snapshot(&self, snapshot: Snapshot) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if index.snapshots.contains_key(&snapshot.snapshot_id) {
            return Err(duplicate("Snapshot", snapshot.snapshot_id));
        }
        self.write_doc("snapshots", snapshot.snapshot_id, &snapshot)?;
        index.snapshots.insert(snapshot.snapshot_id, snapshot);
        Ok(())
    }

    /// Reads a Snapshot from the in-memory index rebuilt at `open`.
    async fn get_snapshot(&self, snapshot_id: SnapshotId) -> AgentResult<Snapshot> {
        self.index
            .read()
            .await
            .snapshots
            .get(&snapshot_id)
            .cloned()
            .ok_or_else(|| not_found("Snapshot", snapshot_id))
    }

    /// Persists a new Issue document; a duplicate ID returns Duplicate.
    async fn insert_issue(&self, issue: Issue) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if index.issues.contains_key(&issue.issue_id) {
            return Err(duplicate("Issue", issue.issue_id));
        }
        self.write_doc("issues", issue.issue_id, &issue)?;
        index.issues.insert(issue.issue_id, issue);
        Ok(())
    }

    /// Overwrites an existing Issue document; absence returns NotFound.
    async fn update_issue(&self, issue: Issue) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if !index.issues.contains_key(&issue.issue_id) {
            return Err(not_found("Issue", issue.issue_id));
        }
        self.write_doc("issues", issue.issue_id, &issue)?;
        index.issues.insert(issue.issue_id, issue);
        Ok(())
    }

    /// Reads an Issue by ID.
    async fn get_issue(&self, issue_id: IssueId) -> AgentResult<Issue> {
        self.index
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
            .index
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

    /// Persists a new Job document; a duplicate ID returns Duplicate.
    async fn insert_job(&self, job: Job) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if index.jobs.contains_key(&job.job_id) {
            return Err(duplicate("Job", job.job_id));
        }
        self.write_doc("jobs", job.job_id, &job)?;
        index.jobs.insert(job.job_id, job);
        Ok(())
    }

    /// Overwrites an existing Job document; absence returns NotFound.
    async fn update_job(&self, job: Job) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if !index.jobs.contains_key(&job.job_id) {
            return Err(not_found("Job", job.job_id));
        }
        self.write_doc("jobs", job.job_id, &job)?;
        index.jobs.insert(job.job_id, job);
        Ok(())
    }

    /// Reads a Job by ID.
    async fn get_job(&self, job_id: JobId) -> AgentResult<Job> {
        self.index
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
            .index
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

    /// Persists a new ActionRun document; a duplicate ID returns Duplicate.
    async fn insert_action_run(&self, action: ActionRun) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if index.action_runs.contains_key(&action.action_run_id) {
            return Err(duplicate("ActionRun", action.action_run_id));
        }
        self.write_doc("action_runs", action.action_run_id, &action)?;
        index.action_runs.insert(action.action_run_id, action);
        Ok(())
    }

    /// Overwrites an existing ActionRun document; absence returns NotFound.
    async fn update_action_run(&self, action: ActionRun) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if !index.action_runs.contains_key(&action.action_run_id) {
            return Err(not_found("ActionRun", action.action_run_id));
        }
        self.write_doc("action_runs", action.action_run_id, &action)?;
        index.action_runs.insert(action.action_run_id, action);
        Ok(())
    }

    /// Reads an ActionRun by ID.
    async fn get_action_run(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.index
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
            .index
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
            .index
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
        let mut items: Vec<_> = self.index.read().await.issues.values().cloned().collect();
        items.sort_by_key(|item| (item.created_at, item.issue_id));
        Ok(items)
    }

    /// Returns every Job in stable creation-time and ID order.
    async fn list_jobs(&self) -> AgentResult<Vec<Job>> {
        let mut items: Vec<_> = self.index.read().await.jobs.values().cloned().collect();
        items.sort_by_key(|item| (item.created_at, item.job_id));
        Ok(items)
    }

    /// Returns every Snapshot in stable creation-time and ID order.
    async fn list_snapshots(&self) -> AgentResult<Vec<Snapshot>> {
        let mut items: Vec<_> = self
            .index
            .read()
            .await
            .snapshots
            .values()
            .cloned()
            .collect();
        items.sort_by_key(|item| (item.created_at, item.snapshot_id));
        Ok(items)
    }

    /// Persists immutable Artifact metadata; a duplicate ID returns Duplicate.
    async fn insert_artifact(&self, artifact: Artifact) -> AgentResult<()> {
        let mut index = self.index.write().await;
        if index.artifacts.contains_key(&artifact.artifact_id) {
            return Err(duplicate("Artifact", artifact.artifact_id));
        }
        self.write_doc("artifacts", artifact.artifact_id, &artifact)?;
        index.artifacts.insert(artifact.artifact_id, artifact);
        Ok(())
    }

    /// Reads Artifact metadata by ID.
    async fn get_artifact(&self, artifact_id: ArtifactId) -> AgentResult<Artifact> {
        self.index
            .read()
            .await
            .artifacts
            .get(&artifact_id)
            .cloned()
            .ok_or_else(|| not_found("Artifact", artifact_id))
    }

    /// Assigns the next sequence under the write lock and appends the line before releasing it.
    ///
    /// Holding the lock across the file append keeps the on-disk order identical to the sequence
    /// order under concurrent writers.
    async fn append_event(&self, event: NewEvent) -> AgentResult<EventRecord> {
        let mut index = self.index.write().await;
        let sequence = index.next_event_sequence;
        let record = EventRecord::from_new(sequence, event);
        self.append_event_line(&record)?;
        index.next_event_sequence += 1;
        index.events.push(record.clone());
        Ok(record)
    }

    /// Returns the complete EventLog in sequence order.
    async fn list_events(&self) -> AgentResult<Vec<EventRecord>> {
        Ok(self.index.read().await.events.clone())
    }
}
