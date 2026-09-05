//! Topology-driven Collector: probes real endpoints and builds immutable Snapshots.
//!
//! The Probe Registry contains six read-only probes:
//!
//! - `tcp.connect` — opens a TCP connection to `host:port` and reports latency.
//! - `http.status` — issues a minimal plain-HTTP `GET` and reports the status code. HTTPS is out
//!   of scope for this probe and is recorded as a coverage gap, not silently skipped.
//! - `redis.llen` — reads one list length over the plain Redis protocol (queue backlog).
//! - `http.json` — reads one value at a JSON pointer from a plain-HTTP `GET`.
//! - `broccoli.worker` — reads one worker's heartbeat from Broccoli's admin API: a worker has no
//!   inbound port, so this is the only honest way to see it. Live heartbeat is `Healthy`, a
//!   stale one `Degraded`, none `Down`; in-flight count and heartbeat age become metrics.
//! - `broccoli.queue` — reads one MQ queue's depth from the same API's overview.
//!
//! The first four are unauthenticated reads. The Broccoli probes log in with credentials taken
//! from the environment variable the probe names — never from the topology file — and cache the
//! JWT per server, re-logging in once on a 401. No probe mutates anything or runs a shell.
//! Latency thresholds and value ranges in the probe spec turn a number into `Degraded` or
//! `Down`, so "reachable but backed up" is visible and verifiable, not just "port open". A
//! resource with no runnable probes is `Unknown` with an explicit coverage gap — the design
//! treats "we cannot see it" as a first-class fact, never as healthy.

use std::collections::HashMap;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

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
/// Reads one worker's heartbeat from Broccoli's admin API.
pub const PROBE_BROCCOLI_WORKER: &str = "broccoli.worker";
/// Reads one MQ queue's depth from Broccoli's admin API.
pub const PROBE_BROCCOLI_QUEUE: &str = "broccoli.queue";
/// Every registered probe ID.
pub const PROBE_REGISTRY: [&str; 6] = [
    PROBE_TCP_CONNECT,
    PROBE_HTTP_STATUS,
    PROBE_REDIS_LLEN,
    PROBE_HTTP_JSON,
    PROBE_BROCCOLI_WORKER,
    PROBE_BROCCOLI_QUEUE,
];
/// Environment variable the Broccoli probes read `username:password` from unless a probe names
/// another one.
pub const DEFAULT_LOGIN_ENV: &str = "BROCCOLI_PROBE_LOGIN";
/// Broccoli's login endpoint, relative to the server base URL.
const BROCCOLI_LOGIN_PATH: &str = "/api/v1/auth/login";
/// Broccoli's worker list, relative to the server base URL; needs `system:view`.
const BROCCOLI_WORKERS_PATH: &str = "/api/v1/admin/system/workers";
/// Broccoli's system overview (workers, queues, in-progress counts); needs `system:view`.
const BROCCOLI_OVERVIEW_PATH: &str = "/api/v1/admin/system/overview";

/// Result of running one probe against one resource.
#[derive(Debug, Clone)]
struct ProbeOutcome {
    probe_id: String,
    succeeded: bool,
    /// The probe answered, but outside its healthy range or latency threshold.
    degraded: bool,
    latency_ms: f64,
    /// Business values the probe read, published as metrics.
    values: Vec<(String, f64, &'static str)>,
    /// Extra facts the probe read (remote text: published under the `probe.` prefix so the View
    /// fences them as untrusted data).
    facts: Vec<(String, String)>,
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
            values: Vec::new(),
            facts: Vec::new(),
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
        self.values.push((name.clone(), value, unit));
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
    /// Secrets the Broccoli probes need, keyed by the environment variable name that supplied
    /// them. Read once at construction; never written anywhere.
    secrets: HashMap<String, String>,
    http: reqwest::Client,
    /// Cached bearer tokens per server base URL.
    tokens: Mutex<HashMap<String, String>>,
}

impl TopologyCollector {
    /// Creates a Collector over the given topology, writing probe evidence to the store.
    ///
    /// Every environment variable the topology's Broccoli probes name (and the default
    /// `BROCCOLI_PROBE_LOGIN`) is read now, so a missing credential shows up as a probe failure
    /// with the variable's name rather than as a mystery later.
    pub fn new(topology: DeploymentTopology, store: Arc<dyn StateStore>) -> Self {
        let timeout = Duration::from_secs(3);
        let mut secrets = HashMap::new();
        let names = topology
            .resources
            .iter()
            .flat_map(|resource| resource.probes.iter())
            .flat_map(|spec| [spec.login_env.clone(), spec.token_env.clone()])
            .flatten()
            .chain(std::iter::once(DEFAULT_LOGIN_ENV.to_string()));
        for name in names {
            if let Ok(value) = std::env::var(&name) {
                secrets.insert(name, value);
            }
        }
        Self {
            topology,
            store,
            timeout,
            secrets,
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Supplies a secret as if the named environment variable held it (tests, embedding).
    pub fn with_secret(mut self, env_name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.insert(env_name.into(), value.into());
        self
    }

    /// Returns a bearer token for the server, logging in with the probe's credentials when
    /// none is cached (or when `refresh` says the cached one was rejected).
    async fn session_token(
        &self,
        base: &str,
        spec: &ProbeSpec,
        refresh: bool,
    ) -> Result<String, String> {
        if let Some(token_env) = &spec.token_env {
            return self.secrets.get(token_env).cloned().ok_or_else(|| {
                format!(
                    "set environment variable `{token_env}` to a Broccoli API token with \
                     system:view"
                )
            });
        }
        if !refresh && let Some(token) = self.tokens.lock().await.get(base) {
            return Ok(token.clone());
        }
        let login_env = spec.login_env.as_deref().unwrap_or(DEFAULT_LOGIN_ENV);
        let login = self.secrets.get(login_env).ok_or_else(|| {
            format!(
                "set environment variable `{login_env}` to `username:password` of a Broccoli \
                 account with system:view"
            )
        })?;
        let (username, password) = login
            .split_once(':')
            .ok_or_else(|| format!("`{login_env}` must be `username:password`"))?;
        let response = self
            .http
            .post(format!("{base}{BROCCOLI_LOGIN_PATH}"))
            .json(&json!({ "username": username, "password": password }))
            .send()
            .await
            .map_err(|error| format!("login request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("login as `{username}` failed: HTTP {status}"));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| format!("login response is not JSON: {error}"))?;
        let token = body["token"]
            .as_str()
            .ok_or_else(|| "login response has no `token`".to_string())?
            .to_string();
        self.tokens
            .lock()
            .await
            .insert(base.to_string(), token.clone());
        Ok(token)
    }

    /// Reads one JSON document from Broccoli's API, re-logging in once on a rejected token.
    async fn broccoli_get(&self, url: &str, path: &str, spec: &ProbeSpec) -> Result<Value, String> {
        let base = url.trim_end_matches('/');
        let mut token = self.session_token(base, spec, false).await?;
        for attempt in 0..2 {
            let response = self
                .http
                .get(format!("{base}{path}"))
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|error| format!("request to {base}{path} failed: {error}"))?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                && attempt == 0
                && spec.token_env.is_none()
            {
                token = self.session_token(base, spec, true).await?;
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP {status} from {base}{path}"));
            }
            return response
                .json()
                .await
                .map_err(|error| format!("response from {base}{path} is not JSON: {error}"));
        }
        Err("token rejected twice".to_string())
    }

    /// Runs one probe spec and classifies the outcome; never panics on bad specs.
    async fn run_probe(&self, resource_id: &str, spec: &ProbeSpec) -> Result<ProbeOutcome, String> {
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
            PROBE_BROCCOLI_WORKER => {
                let url = spec.url.as_deref().ok_or_else(|| {
                    "broccoli.worker requires `url = \"http://server:port\"`".to_string()
                })?;
                let worker_id = spec
                    .worker_id
                    .clone()
                    .unwrap_or_else(|| resource_id.to_string());
                let document = match self.broccoli_get(url, BROCCOLI_WORKERS_PATH, spec).await {
                    Ok(document) => document,
                    Err(error) => {
                        return Ok(ProbeOutcome::new(
                            &spec.probe,
                            false,
                            elapsed(),
                            format!("workers API: {error}"),
                        ));
                    }
                };
                let Some(worker) = document["workers"].as_array().and_then(|workers| {
                    workers
                        .iter()
                        .find(|worker| worker["id"].as_str() == Some(worker_id.as_str()))
                }) else {
                    return Ok(ProbeOutcome::new(
                        &spec.probe,
                        false,
                        elapsed(),
                        format!("no heartbeat from `{worker_id}` in the last 15 s"),
                    ));
                };
                let stale = worker["stale"].as_bool().unwrap_or(true);
                let age = worker["seconds_since_last_seen"]
                    .as_f64()
                    .unwrap_or(f64::NAN);
                let in_flight = worker["in_flight"].as_f64().unwrap_or(0.0);
                let text = |key: &str| worker[key].as_str().unwrap_or("?").to_string();
                let mut outcome = ProbeOutcome::new(
                    &spec.probe,
                    true,
                    elapsed(),
                    format!(
                        "heartbeat {age:.0} s ago, {in_flight} in flight, version {}, {} on {}",
                        text("version"),
                        text("sandbox_backend"),
                        text("hostname")
                    ),
                );
                if stale {
                    outcome.degraded = true;
                    outcome.detail.push_str(" (stale)");
                }
                outcome
                    .values
                    .push(("worker.in_flight".to_string(), in_flight, "tasks"));
                outcome
                    .values
                    .push(("worker.heartbeat_age".to_string(), age, "s"));
                if let Some(max) = worker["max_concurrency"].as_f64() {
                    outcome
                        .values
                        .push(("worker.max_concurrency".to_string(), max, "tasks"));
                }
                for key in ["version", "hostname", "sandbox_backend", "os", "arch"] {
                    if let Some(value) = worker[key].as_str() {
                        outcome
                            .facts
                            .push((format!("probe.broccoli.worker.{key}"), value.to_string()));
                    }
                }
                Ok(outcome)
            }
            PROBE_BROCCOLI_QUEUE => {
                let url = spec.url.as_deref().ok_or_else(|| {
                    "broccoli.queue requires `url = \"http://server:port\"`".to_string()
                })?;
                let queue = spec.queue.as_deref().ok_or_else(|| {
                    "broccoli.queue requires `queue = \"<queue name>\"`".to_string()
                })?;
                let document = match self.broccoli_get(url, BROCCOLI_OVERVIEW_PATH, spec).await {
                    Ok(document) => document,
                    Err(error) => {
                        return Ok(ProbeOutcome::new(
                            &spec.probe,
                            false,
                            elapsed(),
                            format!("overview API: {error}"),
                        ));
                    }
                };
                let queues = document["queues"].as_array().cloned().unwrap_or_default();
                let Some(entry) = queues
                    .iter()
                    .find(|entry| entry["name"].as_str() == Some(queue))
                else {
                    let known: Vec<_> = queues
                        .iter()
                        .filter_map(|entry| entry["name"].as_str())
                        .collect();
                    return Ok(ProbeOutcome::new(
                        &spec.probe,
                        false,
                        elapsed(),
                        format!(
                            "queue `{queue}` is not in the overview (known: {})",
                            known.join(", ")
                        ),
                    ));
                };
                let depth = entry["depth"].as_f64().unwrap_or(0.0);
                let breakdown = entry["breakdown"]
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(state, count)| format!("{state}={count}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let mut outcome = ProbeOutcome::new(
                    &spec.probe,
                    true,
                    elapsed(),
                    format!("queue {queue} depth {depth} ({breakdown})"),
                )
                .with_value(spec, depth, "messages", "queue.depth");
                if let Some(count) = document["submissions_in_progress"].as_f64() {
                    outcome.values.push((
                        "broccoli.submissions_in_progress".to_string(),
                        count,
                        "submissions",
                    ));
                }
                if let Some(count) = document["dlq_unresolved_count"].as_f64() {
                    outcome
                        .values
                        .push(("broccoli.dlq_unresolved".to_string(), count, "messages"));
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
            match self.run_probe(&resource.id, spec).await {
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
            for (name, value, unit) in &outcome.values {
                state
                    .metrics
                    .push(Metric::new(name.clone(), *value, *unit, 0));
            }
            for (name, value) in &outcome.facts {
                state
                    .facts
                    .push(NamedValue::new(name.clone(), value.clone()));
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
                            "values": o.values.iter().map(|(name, value, unit)| json!({
                                "metric": name, "value": value, "unit": unit,
                            })).collect::<Vec<_>>(),
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
