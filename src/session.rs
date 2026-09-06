//! Session files: an Issue with everything that happened on it, as one JSON document.
//!
//! A "session" is the Issue and its pass chain — the Jobs, the ActionRuns their proposals
//! became, the Snapshots each pass reasoned over, the Artifacts (every Snapshot View the model
//! read, every pass transcript, every execution record) with their bodies, and every event bound
//! to any of them. Exporting is a read. Importing loads the same records, under a
//! [`SessionProvenance`], as a read-only archive: the consoles show it like any other Issue and
//! open its traces, and every control decision — dispatch, approval, review, closure, recovery,
//! spend — ignores it.
//!
//! Bodies travel as readable JSON when they are JSON (every body the control plane writes is),
//! and as base64 otherwise. Either way the recorded SHA-256 is checked on import, so a session
//! file edited in transit is refused rather than archived as if genuine.

use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::domain::{
    ActionRun, Artifact, ArtifactId, EventRecord, Issue, IssueId, Job, NewEvent, SessionProvenance,
    Snapshot, SnapshotId,
};
use crate::error::{AgentError, AgentResult};
use crate::i18n;
use crate::ports::StateStore;
use crate::tr;
use crate::view::FileArtifactStore;

/// The `format` field every session file carries.
pub const SESSION_FORMAT: &str = "broccoli-devops-agent.session";
/// The `version` this code writes and accepts.
pub const SESSION_VERSION: u32 = 1;

/// An Artifact body inside a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "content", rename_all = "snake_case")]
pub enum ArtifactBody {
    /// The body as the JSON document it is, readable in the file.
    Json(Value),
    /// Any other bytes, base64-encoded.
    Base64(String),
}

impl ArtifactBody {
    /// JSON when the bytes parse and re-serialize to exactly themselves — so the hash can be
    /// checked after the round trip — and base64 otherwise.
    fn encode(bytes: &[u8]) -> Self {
        if let Ok(value) = serde_json::from_slice::<Value>(bytes)
            && serde_json::to_vec_pretty(&value).is_ok_and(|again| again == bytes)
        {
            return Self::Json(value);
        }
        Self::Base64(BASE64.encode(bytes))
    }

    /// The bytes the body stands for.
    fn decode(&self) -> AgentResult<Vec<u8>> {
        match self {
            Self::Json(value) => Ok(serde_json::to_vec_pretty(value)?),
            Self::Base64(text) => BASE64.decode(text).map_err(|error| {
                AgentError::InvalidInput(format!("artifact body is not valid base64: {error}"))
            }),
        }
    }
}

/// One Artifact with its body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionArtifact {
    /// The record as stored, `uri` included (it is re-pointed on import).
    pub artifact: Artifact,
    /// The body.
    pub body: ArtifactBody,
}

/// A session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionBundle {
    /// Always [`SESSION_FORMAT`].
    pub format: String,
    /// Always [`SESSION_VERSION`] for files this code writes.
    pub version: u32,
    /// When the file was written.
    pub exported_at: DateTime<Utc>,
    /// Who asked for it.
    pub exported_by: String,
    /// The deployment the Issue belongs to, by name.
    pub deployment: String,
    /// Version of the agent that wrote the file.
    pub agent_version: String,
    /// The agent's output language at the time, so a reader knows what to expect of the text.
    pub language: String,
    /// The Issue.
    pub issue: Issue,
    /// Every pass on it, in creation order.
    pub jobs: Vec<Job>,
    /// Every ActionRun on it, in creation order.
    pub action_runs: Vec<ActionRun>,
    /// Every Snapshot the Issue, a pass, or an action refers to.
    pub snapshots: Vec<Snapshot>,
    /// Every Artifact a pass, an action, or an event refers to, with its body.
    pub artifacts: Vec<SessionArtifact>,
    /// Every event bound to the Issue, one of its Jobs, or one of its ActionRuns, in sequence
    /// order, plus the human report or Judge event that opened it.
    pub events: Vec<EventRecord>,
}

/// What an import stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    /// The archived Issue.
    pub issue_id: IssueId,
    /// Its title.
    pub title: String,
    /// Where it came from.
    pub source_deployment: String,
    /// When it was exported.
    pub exported_at: DateTime<Utc>,
    /// Who exported it.
    pub exported_by: String,
    /// Jobs stored.
    pub jobs: usize,
    /// ActionRuns stored.
    pub action_runs: usize,
    /// Snapshots stored (ones already present, byte for byte, are not counted).
    pub snapshots: usize,
    /// Artifacts stored (likewise).
    pub artifacts: usize,
    /// Events appended, the import's own event excluded.
    pub events: usize,
}

fn push_unique<T: PartialEq + Copy>(list: &mut Vec<T>, id: T) {
    if !list.contains(&id) {
        list.push(id);
    }
}

/// Assembles the session file for an Issue.
///
/// Records the store no longer has (a Snapshot or Artifact that was never registered) are left
/// out rather than failing the export: the file says what is known, and the reader sees the
/// dangling reference as such.
pub async fn export_session(
    store: &dyn StateStore,
    artifacts: &FileArtifactStore,
    issue_id: IssueId,
    exported_by: &str,
    deployment: &str,
) -> AgentResult<SessionBundle> {
    let issue = store.get_issue(issue_id).await?;
    let jobs: Vec<Job> = store
        .list_jobs()
        .await?
        .into_iter()
        .filter(|job| job.issue_id == issue_id)
        .collect();
    let action_runs: Vec<ActionRun> = store
        .list_action_runs()
        .await?
        .into_iter()
        .filter(|action| action.issue_id == issue_id)
        .collect();
    let job_ids: HashSet<_> = jobs.iter().map(|job| job.job_id).collect();
    let action_ids: HashSet<_> = action_runs
        .iter()
        .map(|action| action.action_run_id)
        .collect();
    let events: Vec<EventRecord> = store
        .list_events()
        .await?
        .into_iter()
        .filter(|event| {
            event.issue_id == Some(issue_id)
                || event.event_id == issue.source_event_id
                || event.job_id.is_some_and(|id| job_ids.contains(&id))
                || event
                    .action_run_id
                    .is_some_and(|id| action_ids.contains(&id))
        })
        .collect();

    let mut snapshot_ids: Vec<SnapshotId> = Vec::new();
    push_unique(&mut snapshot_ids, issue.opened_snapshot_id);
    push_unique(&mut snapshot_ids, issue.current_snapshot_id);
    for job in &jobs {
        push_unique(&mut snapshot_ids, job.base_snapshot_id());
    }
    for action in &action_runs {
        push_unique(&mut snapshot_ids, action.before_snapshot_id);
        if let Some(after) = action.after_snapshot_id {
            push_unique(&mut snapshot_ids, after);
        }
    }
    let mut snapshots = Vec::new();
    for id in snapshot_ids {
        match store.get_snapshot(id).await {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(AgentError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }

    let mut artifact_ids: Vec<ArtifactId> = Vec::new();
    for job in &jobs {
        push_unique(&mut artifact_ids, job.snapshot_view.artifact_id);
        if let Some(result) = &job.result {
            for id in &result.artifact_ids {
                push_unique(&mut artifact_ids, *id);
            }
        }
    }
    for action in &action_runs {
        if let Some(id) = action.execution_artifact_id {
            push_unique(&mut artifact_ids, id);
        }
    }
    for event in &events {
        for id in &event.artifact_ids {
            push_unique(&mut artifact_ids, *id);
        }
    }
    let mut bundled = Vec::new();
    for id in artifact_ids {
        let artifact = match store.get_artifact(id).await {
            Ok(artifact) => artifact,
            Err(AgentError::NotFound { .. }) => continue,
            Err(error) => return Err(error),
        };
        let bytes = artifacts.read_verified(&artifact)?;
        bundled.push(SessionArtifact {
            artifact,
            body: ArtifactBody::encode(&bytes),
        });
    }

    Ok(SessionBundle {
        format: SESSION_FORMAT.into(),
        version: SESSION_VERSION,
        exported_at: Utc::now(),
        exported_by: exported_by.into(),
        deployment: deployment.into(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        language: i18n::language().tag().into(),
        issue,
        jobs,
        action_runs,
        snapshots,
        artifacts: bundled,
        events,
    })
}

/// Loads a session file as a read-only archive.
///
/// Everything is checked before anything is written: the format, that the Issue and its Jobs
/// and ActionRuns are not already present, and that every Artifact body hashes to what its
/// record says. Snapshots and Artifacts that are already present byte for byte (the file came
/// from this very store, say) are kept as they are; ones that differ under the same ID are a
/// refusal. Then the immutable records go in first — bodies, Snapshots — and the Issue, Jobs,
/// ActionRuns, and events after, so a crash halfway leaves orphaned immutable records rather than
/// an Issue whose passes are missing. Imported events keep their IDs, times, and content and get
/// new sequence numbers here; the import itself is recorded as one more event on the Issue.
pub async fn import_session(
    store: &dyn StateStore,
    artifacts: &FileArtifactStore,
    bundle: SessionBundle,
    imported_by: &str,
) -> AgentResult<ImportSummary> {
    if bundle.format != SESSION_FORMAT {
        return Err(AgentError::InvalidInput(format!(
            "not a session file: format is `{}`, expected `{SESSION_FORMAT}`",
            bundle.format
        )));
    }
    if bundle.version > SESSION_VERSION {
        return Err(AgentError::InvalidInput(format!(
            "session file version {} is newer than this agent understands ({SESSION_VERSION})",
            bundle.version
        )));
    }
    let issue_id = bundle.issue.issue_id;
    if store.get_issue(issue_id).await.is_ok() {
        return Err(AgentError::Duplicate {
            entity: "Issue",
            id: issue_id.to_string(),
        });
    }
    for job in &bundle.jobs {
        if job.issue_id != issue_id {
            return Err(AgentError::InvalidInput(format!(
                "Job `{}` in the file belongs to Issue `{}`, not `{issue_id}`",
                job.job_id, job.issue_id
            )));
        }
        if store.get_job(job.job_id).await.is_ok() {
            return Err(AgentError::Duplicate {
                entity: "Job",
                id: job.job_id.to_string(),
            });
        }
    }
    for action in &bundle.action_runs {
        if action.issue_id != issue_id {
            return Err(AgentError::InvalidInput(format!(
                "ActionRun `{}` in the file belongs to Issue `{}`, not `{issue_id}`",
                action.action_run_id, action.issue_id
            )));
        }
        if store.get_action_run(action.action_run_id).await.is_ok() {
            return Err(AgentError::Duplicate {
                entity: "ActionRun",
                id: action.action_run_id.to_string(),
            });
        }
    }

    // Decode and verify every body before touching the store.
    let mut bodies: Vec<(Artifact, Vec<u8>)> = Vec::new();
    for item in &bundle.artifacts {
        let bytes = item.body.decode()?;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if hash != item.artifact.content_sha256 || bytes.len() as u64 != item.artifact.size_bytes {
            return Err(AgentError::InvalidInput(format!(
                "artifact `{}` in the file does not match its recorded hash; the file was \
                 altered after export",
                item.artifact.artifact_id
            )));
        }
        bodies.push((item.artifact.clone(), bytes));
    }

    let mut stored_artifacts = 0;
    for (artifact, bytes) in &bodies {
        match store.get_artifact(artifact.artifact_id).await {
            Ok(existing) if existing.content_sha256 == artifact.content_sha256 => continue,
            Ok(_) => {
                return Err(AgentError::InvalidInput(format!(
                    "artifact `{}` already exists here with different content",
                    artifact.artifact_id
                )));
            }
            Err(AgentError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        let restored = artifacts.restore(artifact, bytes)?;
        store.insert_artifact(restored).await?;
        stored_artifacts += 1;
    }

    let mut stored_snapshots = 0;
    for snapshot in &bundle.snapshots {
        match store.get_snapshot(snapshot.snapshot_id).await {
            Ok(existing) if &existing == snapshot => continue,
            Ok(_) => {
                return Err(AgentError::InvalidInput(format!(
                    "Snapshot `{}` already exists here with different content",
                    snapshot.snapshot_id
                )));
            }
            Err(AgentError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        store.insert_snapshot(snapshot.clone()).await?;
        stored_snapshots += 1;
    }

    let now = Utc::now();
    let mut issue = bundle.issue.clone();
    issue.provenance = Some(SessionProvenance {
        source_deployment: bundle.deployment.clone(),
        source_agent_version: bundle.agent_version.clone(),
        exported_at: bundle.exported_at,
        exported_by: bundle.exported_by.clone(),
        imported_at: now,
        imported_by: imported_by.into(),
    });
    store.insert_issue(issue).await?;
    for job in &bundle.jobs {
        store.insert_job(job.clone()).await?;
    }
    for action in &bundle.action_runs {
        store.insert_action_run(action.clone()).await?;
    }
    for event in &bundle.events {
        store
            .append_event(NewEvent {
                event_id: event.event_id,
                occurred_at: event.occurred_at,
                actor: event.actor.clone(),
                kind: event.kind.clone(),
                issue_id: event.issue_id,
                job_id: event.job_id,
                action_run_id: event.action_run_id,
                summary: event.summary.clone(),
                payload: event.payload.clone(),
                artifact_ids: event.artifact_ids.clone(),
                trust: event.trust,
            })
            .await?;
    }
    store
        .append_event(
            NewEvent::new(
                "human",
                "human.session_imported",
                tr!(
                    format!(
                        "{imported_by} imported the session exported by {} from `{}` on {} as a \
                         read-only archive",
                        bundle.exported_by,
                        bundle.deployment,
                        bundle.exported_at.format("%Y-%m-%d %H:%M UTC")
                    ),
                    format!(
                        "{imported_by} 导入了 {} 于 {} 从“{}”导出的会话，作为只读归档",
                        bundle.exported_by,
                        bundle.exported_at.format("%Y-%m-%d %H:%M UTC"),
                        bundle.deployment
                    )
                ),
            )
            .with_issue(issue_id)
            .with_payload(json!({
                "source_deployment": bundle.deployment,
                "source_agent_version": bundle.agent_version,
                "exported_at": bundle.exported_at,
                "exported_by": bundle.exported_by,
                "jobs": bundle.jobs.len(),
                "action_runs": bundle.action_runs.len(),
                "snapshots": stored_snapshots,
                "artifacts": stored_artifacts,
                "events": bundle.events.len(),
            })),
        )
        .await?;

    Ok(ImportSummary {
        issue_id,
        title: bundle.issue.title,
        source_deployment: bundle.deployment,
        exported_at: bundle.exported_at,
        exported_by: bundle.exported_by,
        jobs: bundle.jobs.len(),
        action_runs: bundle.action_runs.len(),
        snapshots: stored_snapshots,
        artifacts: stored_artifacts,
        events: bundle.events.len(),
    })
}
