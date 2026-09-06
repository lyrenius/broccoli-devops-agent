//! What an operator console sees of a pass while it is still running.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One transcript entry of a running pass, forwarded as it is appended.
///
/// The control plane does not know the harness's conversation types — nothing from the harness
/// may cross a port — so the item travels as the JSON the stored transcript will contain, with
/// long text cut down to a preview. The index is the entry's position in that transcript, which
/// lets a live view and the stored Artifact line up entry for entry once the pass is over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    /// Position of the entry in the pass's transcript, counting from zero.
    pub index: usize,
    /// When the entry was appended.
    pub at: DateTime<Utc>,
    /// The entry's item as the transcript serializes it (`type`, `text`, `tool`, `arguments`,
    /// `output`, `trust`, …), with text fields shortened to a preview when they are long.
    pub item: Value,
    /// Whether any field of `item` was shortened; the full text is in the stored transcript.
    pub truncated: bool,
}
