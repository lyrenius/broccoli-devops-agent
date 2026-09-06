//! Sanitized digests of Agents Platform output.
//!
//! Machine output is untrusted: it can carry a credential a runbook printed by accident, or a
//! prompt injection planted in a log line. Before any of it reaches a model — as execution
//! evidence in the next pass's View, or as the answer to a Team's inspection — it is condensed
//! to a tail, secret-shaped lines are dropped, and the whole thing is capped. The caller then
//! fences the digest as untrusted data. The full record stays in the ActionOutput Artifact for
//! humans and replay.

use serde_json::Value;

/// Characters of execution evidence handed to the next pass, at most.
pub const EVIDENCE_LIMIT: usize = 2000;

/// Fact-name or line fragments that mark secret-shaped output; such lines are dropped.
const EVIDENCE_SECRET_MARKERS: [&str; 6] = [
    "password",
    "secret",
    "token",
    "credential",
    "api_key",
    "authorization",
];

/// How much of a record a digest keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceLimits {
    /// Trailing lines kept per stream.
    pub lines: usize,
    /// Characters kept per stream after the line cut.
    pub chars_per_stream: usize,
    /// Characters kept for the whole digest.
    pub total: usize,
}

impl EvidenceLimits {
    /// The short digest that travels in human feedback to the next pass.
    pub const FEEDBACK: Self = Self {
        lines: 12,
        chars_per_stream: 600,
        total: EVIDENCE_LIMIT,
    };
    /// The longer digest a Team's inspection tool returns: enough of a log tail to reason on.
    pub const INSPECTION: Self = Self {
        lines: 80,
        chars_per_stream: 6000,
        total: 8000,
    };
}

/// Condenses an ActionOutput record into the short feedback digest.
pub fn summarize_execution_record(record: &Value) -> String {
    summarize_execution_record_with(record, EvidenceLimits::FEEDBACK)
}

/// Condenses an ActionOutput record into a sanitized digest within the given limits.
///
/// Per executed command: the target, how it ended (exit code, timeout, spawn failure), and the
/// tails of stderr and stdout with secret-shaped lines replaced. Refusals and dry runs are
/// stated up front.
pub fn summarize_execution_record_with(record: &Value, limits: EvidenceLimits) -> String {
    fn tail(text: &str, lines: usize, chars: usize) -> String {
        let kept: Vec<&str> = text
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| {
                let lower = line.to_ascii_lowercase();
                if EVIDENCE_SECRET_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker))
                {
                    "[line redacted: secret-shaped]"
                } else {
                    line
                }
            })
            .collect();
        let joined = kept.join("\n");
        if joined.len() > chars {
            format!("…{}", tail_chars(&joined, chars))
        } else {
            joined
        }
    }

    let mut parts = Vec::new();
    if let Some(refused) = record["refused"].as_str() {
        parts.push(format!("refused: {refused}"));
    }
    if record["dry_run"].as_bool() == Some(true) {
        parts.push("dry run: commands were rendered, not executed".to_string());
        for command in record["commands"].as_array().into_iter().flatten() {
            if let (Some(target), Some(rendered)) = (command[0].as_str(), command[1].as_str()) {
                parts.push(format!("would run on {target}: {rendered}"));
            }
        }
    }
    for run in record["runs"].as_array().into_iter().flatten() {
        let target = run["target"].as_str().unwrap_or("?");
        let status = if run["spawn_error"].is_string() {
            format!(
                "could not start: {}",
                run["spawn_error"].as_str().unwrap_or("")
            )
        } else if run["timed_out"].as_bool() == Some(true) {
            "timed out and was killed".to_string()
        } else {
            format!(
                "exit code {}",
                run["exit_code"]
                    .as_i64()
                    .map_or("none".to_string(), |code| code.to_string())
            )
        };
        let mut line = format!("target {target}: {status}");
        for (name, key) in [("stderr", "stderr"), ("stdout", "stdout")] {
            if let Some(text) = run[key].as_str()
                && !text.trim().is_empty()
            {
                line.push_str(&format!(
                    "; {name} tail:\n{}",
                    tail(text, limits.lines, limits.chars_per_stream)
                ));
            }
        }
        parts.push(line);
    }
    let evidence = parts.join("\n");
    if evidence.len() > limits.total {
        format!("{}…", head_chars(&evidence, limits.total))
    } else {
        evidence
    }
}

/// The last `chars` bytes of `text`, cut on a character boundary so multibyte output (a Chinese
/// log line, say) can never make the cut panic.
fn tail_chars(text: &str, chars: usize) -> &str {
    let mut start = text.len().saturating_sub(chars);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The first `chars` bytes of `text`, cut on a character boundary.
fn head_chars(text: &str, chars: usize) -> &str {
    let mut end = chars.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digests_drop_secret_lines_and_cut_on_char_boundaries() {
        let record = json!({
            "dry_run": false,
            "runs": [{
                "target": "worker-1",
                "exit_code": 1,
                "stdout": "",
                "stderr": "level=error msg=队列已满 队列已满 队列已满\npassword=hunter2\nend",
                "timed_out": false,
                "spawn_error": null,
            }],
        });
        let digest = summarize_execution_record(&record);
        assert!(digest.contains("target worker-1: exit code 1"));
        assert!(digest.contains("[line redacted: secret-shaped]"));
        assert!(!digest.contains("hunter2"));

        let tight = summarize_execution_record_with(
            &record,
            EvidenceLimits {
                lines: 3,
                chars_per_stream: 7,
                total: 41,
            },
        );
        // Both cuts land inside multibyte characters and must not panic.
        assert!(tight.chars().count() <= 42);
        assert!(tight.starts_with("target"));
    }

    #[test]
    fn dry_runs_list_the_rendered_commands() {
        let record = json!({
            "dry_run": true,
            "commands": [["worker-1", "echo restart worker-1"]],
        });
        let digest = summarize_execution_record(&record);
        assert!(digest.contains("dry run"));
        assert!(digest.contains("would run on worker-1: echo restart worker-1"));
    }
}
