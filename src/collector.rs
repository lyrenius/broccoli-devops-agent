//! Topology-driven Collector: probes real endpoints and builds immutable Snapshots.
//!
//! The Probe Registry contains four read-only probes:
//!
//! - `tcp.connect` — opens a TCP connection to `host:port` and reports latency.
//! - `http.status` — issues a minimal plain-HTTP `GET` and reports the status code. HTTPS is out
//!   of scope and is recorded as a coverage gap, not silently skipped.
//! - `redis.llen` — reads one list length over the plain Redis protocol (queue backlog).
//! - `http.json` — reads one value at a JSON pointer from a plain-HTTP `GET` (worker heartbeats,
//!   judging counters — whatever the deployment's API exposes).
//!
//! All four are unauthenticated reads: no credentials, no mutation, no shell. Latency
//! thresholds and value ranges in the probe spec turn a number into `Degraded` or `Down`, so
//! "reachable but backed up" is visible and verifiable, not just "port open". A resource with no
//! runnable probes is `Unknown` with an explicit coverage gap — the design treats "we cannot see
//! it" as a first-class fact, never as healthy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::domain::{
    ContentTrust, CoverageGap, HealthState, Metric, NamedValue, NewEvent, ResourceState, Snapshot,
};
use crate::error::{AgentError, AgentResult};
use crate::ports::{CaptureRequest, CollectorPort, StateStore};
use crate::topology::{DeploymentTopology, ProbeSpec, TopologyResource};

/// Probe IDs the Collector implements.
pub const PROBE_TCP_CONNECT: &str = "tcp.connect";
/// See [`PROBE_TCP_CONNECT`]; this probe issues a plain-HTTP GET and checks the status code.
pub const PROBE_HTTP_STATUS: &str = "http.status";
/// Reads a Redis list length (queue backlog) over the plain protocol.
pub const PROBE_REDIS_LLEN: &str = "redis.llen";
/// Reads one JSON value from a plain-HTTP GET response.
pub const PROBE_HTTP_JSON: &str = "http.json";
/// Every registered probe ID.
pub const PROBE_REGISTRY: [&str; 4] = [
    PROBE_TCP_CONNECT,
    PROBE_HTTP_STATUS,
    PROBE_REDIS_LLEN,
    PROBE_HTTP_JSON,
];

/// Result of running one probe against one resource.
#[derive(Debug, Clone)]
struct ProbeOutcome {
    probe_id: String,
    succeeded: bool,
    /// The probe answered, but outside its healthy range or latency threshold.
    degraded: bool,
    latency_ms: f64,
    /// Business value the probe read, published as a metric.
    value: Option<(String, f64, &'static str)>,
    /// Short, structured detail. Anything echoed from the remote side is untrusted.
    detail: String,
}

impl ProbeOutcome {
    fn new(probe_id: &str, succeeded: bool, latency_ms: f64, detail: String) -> Self {
        Self {
            probe_id: probe_id.to_string(),
            succeeded,
            degraded: false,
            latency_ms,
            value: None,
            detail,
        }
    }

    /// Applies the spec's latency threshold to a reachability probe.
    fn with_latency_threshold(mut self, spec: &ProbeSpec) -> Self {
        if let Some(limit) = spec.degraded_above_ms
            && self.succeeded
            && self.latency_ms > limit
        {
            self.degraded = true;
            self.detail.push_str(&format!(
                " (latency {:.0} ms > {limit:.0} ms)",
                self.latency_ms
            ));
        }
        self
    }

    /// Judges a numeric business value against the spec's range and publishes it as a metric.
    fn with_value(
        mut self,
        spec: &ProbeSpec,
        value: f64,
        unit: &'static str,
        default: &str,
    ) -> Self {
        let name = spec.metric.clone().unwrap_or_else(|| default.to_string());
        self.value = Some((name.clone(), value, unit));
        let below = spec.min.is_some_and(|min| value < min);
        let above = spec.max.is_some_and(|max| value > max);
        if below || above {
            self.degraded = true;
            self.detail.push_str(&format!(
                " ({name} = {value} outside [{}, {}])",
                spec.min.map_or("-".to_string(), |m| m.to_string()),
                spec.max.map_or("-".to_string(), |m| m.to_string())
            ));
        }
        self
    }
}

/// Collector that walks the static topology and probes each resource.
pub struct TopologyCollector {
    topology: DeploymentTopology,
    store: Arc<dyn StateStore>,
    timeout: Duration,
}

impl TopologyCollector {
    /// Creates a Collector over the given topology, writing probe evidence to the store.
    pub fn new(topology: DeploymentTopology, store: Arc<dyn StateStore>) -> Self {
        Self {
            topology,
            store,
            timeout: Duration::from_secs(3),
        }
    }

    /// Runs one probe spec and classifies the outcome; never panics on bad specs.
    async fn run_probe(&self, spec: &ProbeSpec) -> Result<ProbeOutcome, String> {
        let started = std::time::Instant::now();
        let elapsed = || started.elapsed().as_secs_f64() * 1000.0;
        match spec.probe.as_str() {
            PROBE_TCP_CONNECT => {
                let target = spec
                    .target
                    .as_deref()
                    .ok_or_else(|| "tcp.connect requires `target = \"host:port\"`".to_string())?;
                let attempt = tokio::time::timeout(self.timeout, TcpStream::connect(target)).await;
                let (succeeded, detail) = match attempt {
                    Ok(Ok(_stream)) => (true, format!("connected to {target}")),
                    Ok(Err(error)) => (false, format!("connect to {target} failed: {error}")),
                    Err(_) => (false, format!("connect to {target} timed out")),
                };
                Ok(ProbeOutcome::new(&spec.probe, succeeded, elapsed(), detail)
                    .with_latency_threshold(spec))
            }
            PROBE_HTTP_STATUS => {
                let url = spec
                    .url
                    .as_deref()
                    .ok_or_else(|| "http.status requires `url = \"http://...\"`".to_string())?;
                let (host_port, path) = parse_plain_http_url(url)?;
                let attempt = tokio::time::timeout(self.timeout, http_get(&host_port, &path)).await;
                let (succeeded, detail) = match attempt {
                    Ok(Ok((status, _))) => ((200..300).contains(&status), format!("HTTP {status}")),
                    Ok(Err(error)) => (false, format!("request to {url} failed: {error}")),
                    Err(_) => (false, format!("request to {url} timed out")),
                };
                Ok(ProbeOutcome::new(&spec.probe, succeeded, elapsed(), detail)
                    .with_latency_threshold(spec))
            }
            PROBE_REDIS_LLEN => {
                let target = spec
                    .target
                    .as_deref()
                    .ok_or_else(|| "redis.llen requires `target = \"host:port\"`".to_string())?;
                let key = spec
                    .key
                    .as_deref()
                    .ok_or_else(|| "redis.llen requires `key = \"<list key>\"`".to_string())?;
                let attempt = tokio::time::timeout(self.timeout, redis_llen(target, key)).await;
                match attempt {
                    Ok(Ok(length)) => Ok(ProbeOutcome::new(
                        &spec.probe,
                        true,
                        elapsed(),
                        format!("LLEN {key} = {length}"),
                    )
                    .with_value(spec, length as f64, "entries", "queue.depth")),
                    Ok(Err(error)) => Ok(ProbeOutcome::new(
                        &spec.probe,
                        false,
                        elapsed(),
                        format!("LLEN {key} on {target} failed: {error}"),
                    )),
                    Err(_) => Ok(ProbeOutcome::new(
                        &spec.probe,
                        false,
                        elapsed(),
                        format!("LLEN {key} on {target} timed out"),
                    )),
                }
            }
            PROBE_HTTP_JSON => {
                let url = spec
                    .url
                    .as_deref()
                    .ok_or_else(|| "http.json requires `url = \"http://...\"`".to_string())?;
                let pointer = spec.pointer.as_deref().ok_or_else(|| {
                    "http.json requires `pointer = \"/path/to/value\"`".to_string()
                })?;
                let (host_port, path) = parse_plain_http_url(url)?;
                let attempt = tokio::time::timeout(self.timeout, http_get(&host_port, &path)).await;
                let (status, body) = match attempt {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        return Ok(ProbeOutcome::new(
                            &spec.probe,
                            false,
                            elapsed(),
                            format!("request to {url} failed: {error}"),
                        ));
                    }
                    Err(_) => {
                        return Ok(ProbeOutcome::new(
                            &spec.probe,
                            false,
                            elapsed(),
                            format!("request to {url} timed out"),
                        ));
                    }
                };
                if !(200..300).contains(&status) {
                    return Ok(ProbeOutcome::new(
                        &spec.probe,
                        false,
                        elapsed(),
                        format!("HTTP {status} from {url}"),
                    ));
                }
                let document: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(document) => document,
                    Err(error) => {
                        return Ok(ProbeOutcome::new(
                            &spec.probe,
                            false,
                            elapsed(),
                            format!("response from {url} is not JSON: {error}"),
                        ));
                    }
                };
                let Some(value) = document.pointer(pointer) else {
                    return Ok(ProbeOutcome::new(
                        &spec.probe,
                        false,
                        elapsed(),
                        format!("pointer `{pointer}` is absent from the response"),
                    ));
                };
                let rendered = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let mut outcome = ProbeOutcome::new(
                    &spec.probe,
                    true,
                    elapsed(),
                    format!("{pointer} = {rendered}"),
                );
                if let Some(expected) = &spec.expect
                    && &rendered != expected
                {
                    outcome.succeeded = false;
                    outcome.detail.push_str(&format!(" (expected {expected})"));
                }
                if let Some(number) = value.as_f64() {
                    let default = pointer.trim_start_matches('/').replace('/', ".");
                    outcome = outcome.with_value(spec, number, "value", &default);
                }
                Ok(outcome)
            }
            other => Err(format!("probe `{other}` is not in the Probe Registry")),
        }
    }

    /// Probes one resource and folds the outcomes into a ResourceState plus optional gaps.
    async fn observe_resource(
        &self,
        resource: &TopologyResource,
    ) -> AgentResult<(ResourceState, Vec<CoverageGap>)> {
        let mut outcomes = Vec::new();
        let mut gaps = Vec::new();

        for spec in &resource.probes {
            match self.run_probe(spec).await {
                Ok(outcome) => outcomes.push(outcome),
                Err(reason) => gaps.push(CoverageGap {
                    resource_id: resource.id.clone(),
                    probe_id: spec.probe.clone(),
                    reason,
                    last_success_at: None,
                }),
            }
        }
        if resource.probes.is_empty() {
            gaps.push(CoverageGap {
                resource_id: resource.id.clone(),
                probe_id: "none".to_string(),
                reason: "no probes are defined for this resource".to_string(),
                last_success_at: None,
            });
        }

        let health = match (
            outcomes.iter().filter(|o| o.succeeded).count(),
            outcomes.len(),
        ) {
            (_, 0) => HealthState::Unknown,
            (ok, total) if ok == total => {
                if outcomes.iter().any(|o| o.degraded) {
                    HealthState::Degraded
                } else {
                    HealthState::Healthy
                }
            }
            (0, _) => HealthState::Down,
            _ => HealthState::Degraded,
        };

        let mut state = ResourceState::new(
            resource.id.clone(),
            resource.node.clone(),
            resource.kind,
            health,
            Utc::now(),
        );
        for outcome in &outcomes {
            state.facts.push(NamedValue::new(
                format!("probe.{}", outcome.probe_id),
                outcome.detail.clone(),
            ));
            state.metrics.push(Metric::new(
                format!("probe.{}.latency", outcome.probe_id),
                outcome.latency_ms,
                "ms",
                0,
            ));
            if let Some((name, value, unit)) = &outcome.value {
                state
                    .metrics
                    .push(Metric::new(name.clone(), *value, *unit, 0));
            }
        }

        // Evidence: one event per resource observation, carrying every probe outcome. Probe
        // status is system-produced, but detail strings can echo remote text, so trust is Mixed.
        let event = self
            .store
            .append_event(
                NewEvent::new(
                    "collector",
                    "collector.resource_observed",
                    format!("Observed `{}`: {:?}", resource.id, health),
                )
                .with_payload(json!({
                    "resource_id": resource.id,
                    "health": health,
                    "probes": outcomes
                        .iter()
                        .map(|o| json!({
                            "probe": o.probe_id,
                            "succeeded": o.succeeded,
                            "degraded": o.degraded,
                            "latency_ms": o.latency_ms,
                            "value": o.value.as_ref().map(|(name, value, unit)| json!({
                                "metric": name, "value": value, "unit": unit,
                            })),
                            "detail": o.detail,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .with_trust(ContentTrust::Mixed),
            )
            .await?;
        state.evidence_ids.push(event.event_id);
        Ok((state, gaps))
    }
}

#[async_trait]
impl CollectorPort for TopologyCollector {
    /// Probes every topology resource and assembles an unpersisted immutable Snapshot.
    ///
    /// Requested probe IDs outside the v0.1 registry become coverage gaps so a Team asking for an
    /// unimplemented observation sees the refusal in the Snapshot it gets back.
    async fn capture_snapshot(&self, request: CaptureRequest) -> AgentResult<Snapshot> {
        if request.deployment_id != self.topology.deployment.id {
            return Err(AgentError::InvalidInput(format!(
                "capture requested deployment `{}` but this Collector serves `{}`",
                request.deployment_id, self.topology.deployment.id
            )));
        }

        let mut snapshot = Snapshot::new(
            self.topology.deployment.id,
            self.topology.deployment.topology_revision.clone(),
            request.cause,
            request.operation_mode,
        );
        if let Some(parent) = request.parent_snapshot_id {
            snapshot = snapshot.with_parent(parent);
        }
        snapshot.dependencies = self.topology.dependency_edges();

        for resource in &self.topology.resources {
            let (state, gaps) = self.observe_resource(resource).await?;
            snapshot
                .evidence_ids
                .extend(state.evidence_ids.iter().copied());
            snapshot.resources.push(state);
            snapshot.coverage_gaps.extend(gaps);
        }

        for requested in &request.requested_probe_ids {
            if !PROBE_REGISTRY.contains(&requested.as_str()) {
                snapshot.coverage_gaps.push(CoverageGap {
                    resource_id: "deployment".to_string(),
                    probe_id: requested.clone(),
                    reason: "requested probe is not in the Probe Registry".to_string(),
                    last_success_at: None,
                });
            }
        }

        Ok(snapshot)
    }
}

/// Splits a plain-HTTP URL into `host:port` and path, rejecting anything else.
fn parse_plain_http_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "only plain http:// URLs are supported by the v0.1 probe".to_string())?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("URL `{url}` has no host"));
    }
    let host_port = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok((host_port, path.to_string()))
}

/// Bytes of an HTTP response body a probe will read.
const HTTP_BODY_LIMIT: usize = 256 * 1024;

/// Issues one minimal HTTP/1.1 GET and returns the status code and the (capped) body.
///
/// This is deliberately a health probe, not an HTTP client: no redirects, no TLS, no chunked
/// decoding beyond concatenating what the server sends before closing. Anything larger belongs
/// to a future Collector with a real client behind the Probe Registry.
async fn http_get(host_port: &str, path: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(host_port)
        .await
        .map_err(|error| error.to_string())?;
    let host = host_port.split(':').next().unwrap_or(host_port);
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;

    let mut raw = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        let read = stream
            .read(&mut buf)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
        if raw.len() >= HTTP_BODY_LIMIT {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let status_line = text.lines().next().unwrap_or_default();
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
        .ok_or_else(|| format!("unparseable status line `{status_line}`"))?;
    let body = text
        .find("\r\n\r\n")
        .map(|idx| text[idx + 4..].to_string())
        .unwrap_or_default();
    // A chunked body is decoded just enough for JSON probes: strip chunk-size lines.
    let body = if text
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked(&body)
    } else {
        body
    };
    Ok((code, body))
}

/// Concatenates the chunks of a `Transfer-Encoding: chunked` body.
fn decode_chunked(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(line_end) = rest.find("\r\n") {
        let size_line = rest[..line_end]
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let Ok(size) = usize::from_str_radix(size_line, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let Some(chunk) = rest.get(start..start + size) else {
            break;
        };
        out.push_str(chunk);
        rest = rest.get(start + size + 2..).unwrap_or_default();
    }
    out
}

/// Reads one list length over the plain Redis protocol (`LLEN key`), no AUTH.
///
/// A deployment whose Redis requires a password answers `NOAUTH`, which is reported as a failed
/// probe with that reason: credentials never enter the topology, by design.
async fn redis_llen(target: &str, key: &str) -> Result<u64, String> {
    let mut stream = TcpStream::connect(target)
        .await
        .map_err(|error| error.to_string())?;
    let command = format!("*2\r\n$4\r\nLLEN\r\n${}\r\n{key}\r\n", key.len());
    stream
        .write_all(command.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut buf = [0_u8; 256];
    let read = stream
        .read(&mut buf)
        .await
        .map_err(|error| error.to_string())?;
    let reply = String::from_utf8_lossy(&buf[..read]);
    let line = reply.lines().next().unwrap_or_default();
    match line.chars().next() {
        Some(':') => line[1..]
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("unparseable integer reply `{line}`")),
        Some('-') => Err(line[1..].trim().to_string()),
        _ => Err(format!("unexpected reply `{line}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_chunked, parse_plain_http_url};

    #[test]
    fn chunked_bodies_are_concatenated() {
        assert_eq!(
            decode_chunked("5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n"),
            "{\"a\":1}"
        );
    }

    #[test]
    fn plain_http_urls_parse_and_https_is_rejected() {
        assert_eq!(
            parse_plain_http_url("http://127.0.0.1:3000/healthz").unwrap(),
            ("127.0.0.1:3000".to_string(), "/healthz".to_string())
        );
        assert_eq!(
            parse_plain_http_url("http://example.local").unwrap(),
            ("example.local:80".to_string(), "/".to_string())
        );
        assert!(parse_plain_http_url("https://example.local/x").is_err());
    }
}
