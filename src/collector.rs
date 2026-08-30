//! Topology-driven Collector: probes real endpoints and builds immutable Snapshots.
//!
//! The v0.1 Probe Registry contains two read-only probes:
//!
//! - `tcp.connect` — opens a TCP connection to `host:port` and reports latency.
//! - `http.status` — issues a minimal plain-HTTP `GET` and reports the status code. HTTPS is out
//!   of scope for v0.1 and is recorded as a coverage gap, not silently skipped.
//!
//! Both are reachability checks: no credentials, no mutation, no shell. A resource with no
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

/// Probe IDs the v0.1 Collector implements.
pub const PROBE_TCP_CONNECT: &str = "tcp.connect";
/// See [`PROBE_TCP_CONNECT`]; this probe issues a plain-HTTP GET and checks the status code.
pub const PROBE_HTTP_STATUS: &str = "http.status";

/// Result of running one probe against one resource.
#[derive(Debug, Clone)]
struct ProbeOutcome {
    probe_id: String,
    succeeded: bool,
    latency_ms: f64,
    /// Short, structured detail. Anything echoed from the remote side is untrusted.
    detail: String,
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
                Ok(ProbeOutcome {
                    probe_id: spec.probe.clone(),
                    succeeded,
                    latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                    detail,
                })
            }
            PROBE_HTTP_STATUS => {
                let url = spec
                    .url
                    .as_deref()
                    .ok_or_else(|| "http.status requires `url = \"http://...\"`".to_string())?;
                let (host_port, path) = parse_plain_http_url(url)?;
                let attempt =
                    tokio::time::timeout(self.timeout, http_status(&host_port, &path)).await;
                let (succeeded, detail) = match attempt {
                    Ok(Ok(status)) => ((200..300).contains(&status), format!("HTTP {status}")),
                    Ok(Err(error)) => (false, format!("request to {url} failed: {error}")),
                    Err(_) => (false, format!("request to {url} timed out")),
                };
                Ok(ProbeOutcome {
                    probe_id: spec.probe.clone(),
                    succeeded,
                    latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                    detail,
                })
            }
            other => Err(format!("probe `{other}` is not in the v0.1 Probe Registry")),
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
            (ok, total) if ok == total => HealthState::Healthy,
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
                            "latency_ms": o.latency_ms,
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

        let known = [PROBE_TCP_CONNECT, PROBE_HTTP_STATUS];
        for requested in &request.requested_probe_ids {
            if !known.contains(&requested.as_str()) {
                snapshot.coverage_gaps.push(CoverageGap {
                    resource_id: "deployment".to_string(),
                    probe_id: requested.clone(),
                    reason: "requested probe is not in the v0.1 Probe Registry".to_string(),
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

/// Issues one minimal HTTP/1.1 GET and returns the response status code.
///
/// This is deliberately a health probe, not an HTTP client: no redirects, no TLS, no body
/// interpretation. Anything larger belongs to a future Collector with a real client behind the
/// Probe Registry.
async fn http_status(host_port: &str, path: &str) -> Result<u16, String> {
    let mut stream = TcpStream::connect(host_port)
        .await
        .map_err(|error| error.to_string())?;
    let host = host_port.split(':').next().unwrap_or(host_port);
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;

    let mut buf = [0_u8; 512];
    let read = stream
        .read(&mut buf)
        .await
        .map_err(|error| error.to_string())?;
    let head = String::from_utf8_lossy(&buf[..read]);
    let status_line = head.lines().next().unwrap_or_default();
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
        .ok_or_else(|| format!("unparseable status line `{status_line}`"))?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::parse_plain_http_url;

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
