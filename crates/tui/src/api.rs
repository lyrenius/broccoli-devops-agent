//! Minimal typed client for the control plane's HTTP API.

use serde::Deserialize;
use serde_json::{Value, json};

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
    /// Inbox counts.
    #[serde(default)]
    pub inbox: InboxCounts,
    /// Startup recovery summary, when the server recovered at startup.
    #[serde(default)]
    pub recovery: Option<Value>,
    /// Passes running right now, with the Job that can be interrupted.
    #[serde(default)]
    pub running: Vec<RunningPass>,
    /// What the model relay has been asked to do, and what it cost.
    #[serde(default)]
    pub usage: UsageTotals,
}

/// One pass in flight, from `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunningPass {
    /// Job being run; the ID the cancel route takes.
    #[serde(default)]
    pub job_id: String,
    /// Issue it serves.
    #[serde(default)]
    pub issue_id: String,
    /// When the run started (RFC 3339).
    #[serde(default)]
    pub started_at: String,
}

/// Token and cost totals from `/api/status` and `/api/usage`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageTotals {
    /// Model-backed passes counted.
    #[serde(default)]
    pub passes: u32,
    /// Input tokens, cached ones included.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Input plus output.
    #[serde(default)]
    pub total_tokens: u64,
    /// Requests whose response reported no usage.
    #[serde(default)]
    pub requests_without_usage: u32,
    /// Cost under the configured price list, when there is one.
    #[serde(default)]
    pub cost: Option<f64>,
    /// Currency of `cost`.
    #[serde(default)]
    pub currency: Option<String>,
    /// The configured ceiling and how close the totals are to it.
    #[serde(default)]
    pub budget: Option<BudgetStatus>,
}

/// How the totals stand against the configured spend ceiling.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BudgetStatus {
    /// Whether the ceiling has been reached.
    #[serde(default)]
    pub exceeded: bool,
    /// Fraction of the ceiling used, clamped to one.
    #[serde(default)]
    pub used_fraction: f64,
}

impl UsageTotals {
    /// One line for the status bar: tokens, cost, and any gap in the record.
    pub fn one_line(&self) -> String {
        if self.passes == 0 {
            return "no model passes yet".to_string();
        }
        let mut line = format!(
            "{} pass(es) · {} in + {} out = {} tokens",
            self.passes, self.input_tokens, self.output_tokens, self.total_tokens
        );
        if let (Some(cost), Some(currency)) = (self.cost, self.currency.as_deref()) {
            line.push_str(&format!(" · {cost:.4} {currency}"));
        }
        if let Some(budget) = &self.budget {
            line.push_str(&format!(
                " · {:.0}% of budget",
                budget.used_fraction * 100.0
            ));
            if budget.exceeded {
                line.push_str(" (spent — dispatch frozen)");
            }
        }
        if self.requests_without_usage > 0 {
            line.push_str(&format!(
                " · {} request(s) reported no usage",
                self.requests_without_usage
            ));
        }
        line
    }
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
    /// Total events.
    #[serde(default)]
    pub events: usize,
}

/// Inbox counts inside `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InboxCounts {
    /// Actions waiting for approval.
    #[serde(default)]
    pub permission_requests: usize,
    /// Denied actions awaiting review.
    #[serde(default)]
    pub permission_denied: usize,
    /// Failed Jobs awaiting review.
    #[serde(default)]
    pub failed_jobs: usize,
    /// Failed actions awaiting review.
    #[serde(default)]
    pub failed_actions: usize,
    /// Everything waiting for a human.
    #[serde(default)]
    pub total: usize,
}

/// Why an action was denied.
#[derive(Debug, Clone, Deserialize)]
pub struct Denial {
    /// `policy` or `human`.
    #[serde(default)]
    pub source: String,
    /// The rule's rationale or the fixed human-rejection text.
    #[serde(default)]
    pub reason: String,
    /// The human's comment, if any.
    #[serde(default)]
    pub comment: Option<String>,
    /// Who decided, for human denials.
    #[serde(default)]
    pub decided_by: Option<String>,
}

/// One ActionRun as served by `/api/actions` and `/api/inbox`.
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
    /// Denial, when denied.
    #[serde(default)]
    pub denial: Option<Denial>,
    /// Review, once given (rendered raw).
    #[serde(default)]
    pub review: Option<Value>,
    /// The Platform's own summary of the execution.
    #[serde(default)]
    pub execution_summary: Option<String>,
    /// Verification conclusion, when reached.
    #[serde(default)]
    pub verification_summary: Option<String>,
    /// How much the verification proves: `dry_run`, `weak`, or `strong`.
    #[serde(default)]
    pub verification_evidence: Option<String>,
}

/// One Job as served inside `/api/inbox`.
#[derive(Debug, Clone, Deserialize)]
pub struct JobRow {
    /// Job ID.
    pub job_id: String,
    /// Owning Issue ID.
    #[serde(default)]
    pub issue_id: String,
    /// Lifecycle status.
    #[serde(default)]
    pub status: String,
    /// The Team's result, when any.
    #[serde(default)]
    pub result: Option<JobResultRow>,
}

/// The result summary of a Job.
#[derive(Debug, Clone, Deserialize)]
pub struct JobResultRow {
    /// Team summary.
    #[serde(default)]
    pub summary: String,
}

/// `/api/inbox`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Inbox {
    /// Actions waiting for approval.
    #[serde(default)]
    pub permission_requests: Vec<Action>,
    /// Denied actions awaiting review.
    #[serde(default)]
    pub permission_denied: Vec<Action>,
    /// Failed Jobs awaiting review.
    #[serde(default)]
    pub failed_jobs: Vec<JobRow>,
    /// Failed actions awaiting review.
    #[serde(default)]
    pub failed_actions: Vec<Action>,
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
    /// Business metrics (everything but probe latencies), rendered as `name value`.
    pub signals: String,
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

    async fn post_json(&self, path: &str, body: Option<Value>) -> Result<Value, String> {
        let mut request = self.request(reqwest::Method::POST, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
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

    /// Fetches the inbox.
    pub async fn inbox(&self) -> Result<Inbox, String> {
        self.get_json("/api/inbox").await
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
                        signals: r["metrics"]
                            .as_array()
                            .map(|metrics| {
                                metrics
                                    .iter()
                                    .filter(|m| {
                                        !m["name"].as_str().unwrap_or("").starts_with("probe.")
                                    })
                                    .map(|m| {
                                        format!(
                                            "{} {}",
                                            m["name"].as_str().unwrap_or("?"),
                                            m["value"].as_f64().unwrap_or(0.0)
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" · ")
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Approves an ActionRun in the given operator's name.
    pub async fn approve(&self, id: &str, by: &str) -> Result<Value, String> {
        self.post_json(
            &format!("/api/actions/{id}/approve"),
            Some(json!({ "by": by })),
        )
        .await
    }

    /// Rejects an ActionRun with a comment.
    pub async fn reject(&self, id: &str, by: &str, comment: &str) -> Result<Value, String> {
        self.post_json(
            &format!("/api/actions/{id}/reject"),
            Some(json!({ "by": by, "comment": comment })),
        )
        .await
    }

    /// Reviews a denied or failed ActionRun: `acknowledge` or `send_upstream`.
    pub async fn review_action(
        &self,
        id: &str,
        by: &str,
        decision: &str,
        comment: &str,
    ) -> Result<Value, String> {
        self.post_json(
            &format!("/api/actions/{id}/review"),
            Some(json!({ "by": by, "decision": decision, "comment": comment })),
        )
        .await
    }

    /// Reviews a failed Job: `acknowledge` or `send_upstream`.
    pub async fn review_job(
        &self,
        id: &str,
        by: &str,
        decision: &str,
        comment: &str,
    ) -> Result<Value, String> {
        self.post_json(
            &format!("/api/jobs/{id}/review"),
            Some(json!({ "by": by, "decision": decision, "comment": comment })),
        )
        .await
    }

    /// Closes an Issue: `resolved`, `cancelled`, or `failed`.
    pub async fn close_issue(
        &self,
        id: &str,
        by: &str,
        outcome: &str,
        comment: &str,
    ) -> Result<Value, String> {
        self.post_json(
            &format!("/api/issues/{id}/close"),
            Some(json!({ "by": by, "outcome": outcome, "comment": comment })),
        )
        .await
    }

    /// Requests a Scheduler transition: `freeze-dispatch`, `freeze-all`, or `resume`.
    pub async fn transition(&self, transition: &str) -> Result<Value, String> {
        self.post_json(&format!("/api/scheduler/{transition}"), None)
            .await
    }

    /// Captures a new Snapshot.
    pub async fn capture(&self) -> Result<Value, String> {
        self.post_json("/api/snapshots", None).await
    }

    /// Interrupts a running pass in the given operator's name.
    pub async fn cancel_job(&self, id: &str, by: &str) -> Result<Value, String> {
        self.post_json(&format!("/api/jobs/{id}/cancel"), Some(json!({ "by": by })))
            .await
    }
}
