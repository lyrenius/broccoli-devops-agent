//! Awaited accounting hooks. Display observers are best-effort; request records are durable gates.

use crate::{HarnessResult, Usage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle of a single model request attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    /// Started but no response has been recorded yet.
    Started,
    /// A response arrived, including a response without usable assistant items.
    Succeeded,
    /// The backend returned an error or the transport failed.
    Failed,
    /// A request was abandoned after the model client was polled.
    Cancelled,
    /// Cancelled or refused locally before entering the model client; not an API request.
    NotSent,
}

/// One attempt's timing and usage. IDs are monotonically increasing within one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Run-local stable attempt identifier; adapters namespace it by Job or diagnostic run.
    pub request_id: u32,
    /// Model turn this attempt belongs to, retries included.
    pub turn: u32,
    /// When dispatch was attempted.
    pub started_at: DateTime<Utc>,
    /// When the attempt ended; absent for the start record.
    pub finished_at: Option<DateTime<Utc>>,
    /// Request outcome.
    pub status: RequestStatus,
    /// Reported counts or an explicit unknown-usage request; absent before completion.
    pub usage: Option<Usage>,
    /// Error or cancellation explanation.
    pub error: Option<String>,
}

/// Persists start/finish records before the loop may issue another request.
#[async_trait]
pub trait RequestObserver: Send + Sync {
    /// A budget or policy gate before starting an attempt. A refusal is not counted as a call.
    async fn before_request(&self) -> HarnessResult<()> {
        Ok(())
    }
    /// Records the attempt; persistence failure stops the loop with its partial report intact.
    async fn record(&self, record: &RequestRecord) -> HarnessResult<()>;
}

/// Optional observers for one run, keeping progress separate from required accounting writes.
#[derive(Default, Clone, Copy)]
pub struct RunObservers<'a> {
    /// Live trace/progress display.
    pub progress: Option<&'a dyn crate::ProgressObserver>,
    /// Awaited request accounting and budget checks.
    pub requests: Option<&'a dyn RequestObserver>,
}
