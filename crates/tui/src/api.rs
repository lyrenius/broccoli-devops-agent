//! Minimal typed client for the control plane's HTTP API.

use serde::Deserialize;
use serde_json::Value;

/// One row of `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Status {
    /// Scheduler mode, e.g. `running` or `dispatch_frozen`.
    #[serde(default)]
    pub mode: String,
    /// Wired Team backend label.
    #[serde(default)]
    pub team_backend: String,
    /// Whether the Platform is in dry-run mode.
    #[serde(default)]
    pub dry_run: bool,
    /// Object counts.
    #[serde(default)]
    pub counts: Counts,
}

/// Object counts inside `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Counts {
    /// Total Issues.
    #[serde(default)]
    pub issues: usize,
    /// Total Jobs.
    #[serde(default)]
    pub jobs: usize,
    /// Total ActionRuns.
    #[serde(default)]
    pub actions: usize,
    /// ActionRuns waiting for a human.
    #[serde(default)]
    pub actions_waiting: usize,
    /// Total events.
    #[serde(default)]
    pub events: usize,
}

/// One ActionRun as served by `/api/actions`.
#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    /// ActionRun ID.
    pub action_run_id: String,
    /// Runbook ID.
    pub runbook_id: String,
    /// Target resource IDs.
    #[serde(default)]
    pub target_ids: Vec<String>,
    /// Lifecycle status.
    pub status: String,
    /// Approval state.
    pub approval: String,
    /// Why the Team proposed it.
    #[serde(default)]
    pub reason: String,
    /// Verification conclusion, when reached.
    #[serde(default)]
    pub verification_summary: Option<String>,
}

/// One event record as served by `/api/events`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventRow {
    /// Sequence number.
    pub sequence: u64,
    /// When it occurred (RFC 3339).
    pub occurred_at: String,
    /// Event kind.
    pub kind: String,
    /// Producing component.
    pub actor: String,
    /// Short summary.
    pub summary: String,
}

/// One Issue as served by `/api/issues`.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueRow {
    /// Issue ID.
    pub issue_id: String,
    /// Title.
    pub title: String,
    /// Priority.
    pub priority: String,
    /// Status.
    pub status: String,
}

/// One resource inside the latest Snapshot.
#[derive(Debug, Clone)]
pub struct ResourceRow {
    /// Resource ID.
    pub id: String,
    /// Resource kind.
    pub kind: String,
    /// Health state.
    pub health: String,
}

/// HTTP client bound to one API base URL and optional token.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl ApiClient {
    /// Creates a client for the given base URL (no trailing slash needed).
    pub fn new(base: &str, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        self.request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())
    }

    async fn post_json(&self, path: &str) -> Result<Value, String> {
        let response = self
            .request(reqwest::Method::POST, path)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            Ok(body)
        } else {
            Err(body["error"]
                .as_str()
                .map_or_else(|| status.to_string(), ToString::to_string))
        }
    }

    /// Fetches `/api/status`.
    pub async fn status(&self) -> Result<Status, String> {
        self.get_json("/api/status").await
    }

    /// Fetches every ActionRun.
    pub async fn actions(&self) -> Result<Vec<Action>, String> {
        self.get_json("/api/actions").await
    }

    /// Fetches every Issue.
    pub async fn issues(&self) -> Result<Vec<IssueRow>, String> {
        self.get_json("/api/issues").await
    }

    /// Fetches the newest `limit` events.
    pub async fn events(&self, limit: usize) -> Result<Vec<EventRow>, String> {
        self.get_json(&format!("/api/events?limit={limit}")).await
    }

    /// Fetches the latest Snapshot's resource rows; an absent Snapshot yields an empty list.
    pub async fn latest_resources(&self) -> Result<Vec<ResourceRow>, String> {
        let snapshot: Value = match self.get_json("/api/snapshots/latest").await {
            Ok(value) => value,
            Err(error) if error.contains("404") => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(snapshot["resources"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|r| ResourceRow {
                        id: r["resource_id"].as_str().unwrap_or("?").to_string(),
                        kind: r["kind"].as_str().unwrap_or("?").to_string(),
                        health: r["health"].as_str().unwrap_or("?").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Approves an ActionRun.
    pub async fn approve(&self, id: &str) -> Result<Value, String> {
        self.post_json(&format!("/api/actions/{id}/approve")).await
    }

    /// Rejects an ActionRun.
    pub async fn reject(&self, id: &str) -> Result<Value, String> {
        self.post_json(&format!("/api/actions/{id}/reject")).await
    }

    /// Requests a Scheduler transition: `freeze-dispatch`, `freeze-all`, or `resume`.
    pub async fn transition(&self, transition: &str) -> Result<Value, String> {
        self.post_json(&format!("/api/scheduler/{transition}"))
            .await
    }

    /// Captures a new Snapshot.
    pub async fn capture(&self) -> Result<Value, String> {
        self.post_json("/api/snapshots").await
    }
}
