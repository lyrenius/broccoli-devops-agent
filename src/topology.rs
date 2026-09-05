//! Static deployment topology: which resources exist, where they live, and how to probe them.
//!
//! The topology is the Collector's map of the deployment. It is loaded once from a TOML file the
//! operator maintains, and its revision string is stamped into every Snapshot so post-contest
//! review knows which map produced which observation. The topology never contains credentials —
//! probes are reachability and health checks, not authenticated sessions.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::{DependencyEdge, DeploymentId, OperationMode, ResourceId, ResourceKind};
use crate::error::{AgentError, AgentResult};

/// Identity block of a topology file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentInfo {
    /// Stable ID of the deployment instance.
    pub id: DeploymentId,
    /// Human-readable deployment name.
    pub name: String,
    /// Revision string stamped into every Snapshot built from this topology.
    pub topology_revision: String,
    /// Operation phase this topology currently describes.
    pub operation_mode: OperationMode,
}

/// One probe attached to a resource in the topology.
///
/// `probe` names an entry in the Probe Registry (§4.9 of the architecture document); the
/// registry is the fixed set the Collector implements. Unknown probe names become coverage gaps,
/// never errors, so a stale topology degrades visibly instead of failing collection.
///
/// Reachability probes (`tcp.connect`, `http.status`) take a `target` or `url` and an optional
/// `degraded_above_ms` latency threshold. Generic business probes read one number from the
/// deployment: `redis.llen` (queue backlog of `key`) and `http.json` (a JSON `pointer` in a GET
/// response). Broccoli probes read the server's own admin API: `broccoli.worker` (the worker's
/// heartbeat, in-flight count, and version, matched by `worker_id`) and `broccoli.queue` (the
/// depth of one MQ `queue`). All of them publish numbers as the metric named by `metric` and
/// judge health with `min`, `max`, or `expect`.
///
/// The Broccoli admin API needs a login with `system:view`. The topology never holds it: the
/// probe names the environment variable (`login_env`, default `BROCCOLI_PROBE_LOGIN`) whose value
/// is `username:password`, or `token_env` for a ready bearer token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeSpec {
    /// Registered probe ID, such as `tcp.connect`, `http.status`, `redis.llen`, `http.json`,
    /// `broccoli.worker`, or `broccoli.queue`.
    pub probe: String,
    /// `host:port` target for socket-level probes.
    #[serde(default)]
    pub target: Option<String>,
    /// Plain-HTTP URL for `http.status` and `http.json`; HTTPS is not supported.
    #[serde(default)]
    pub url: Option<String>,
    /// Redis key whose list length `redis.llen` reads.
    #[serde(default)]
    pub key: Option<String>,
    /// RFC 6901 JSON pointer `http.json` reads from the response, e.g. `/workers/online`.
    #[serde(default)]
    pub pointer: Option<String>,
    /// Metric name the value is published under, e.g. `queue.depth` or `workers.online`.
    #[serde(default)]
    pub metric: Option<String>,
    /// Latency above which a reachability probe reports the resource as Degraded.
    #[serde(default)]
    pub degraded_above_ms: Option<f64>,
    /// Lowest healthy value for a numeric business probe.
    #[serde(default)]
    pub min: Option<f64>,
    /// Highest healthy value for a numeric business probe.
    #[serde(default)]
    pub max: Option<f64>,
    /// Exact expected value (string comparison) for `http.json`.
    #[serde(default)]
    pub expect: Option<String>,
    /// Worker ID to look for in Broccoli's worker list; defaults to the resource ID.
    #[serde(default)]
    pub worker_id: Option<String>,
    /// MQ queue name for `broccoli.queue`, e.g. `operation_tasks`.
    #[serde(default)]
    pub queue: Option<String>,
    /// Environment variable holding `username:password` for the Broccoli API probes.
    #[serde(default)]
    pub login_env: Option<String>,
    /// Environment variable holding a ready bearer token for the Broccoli API probes.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl ProbeSpec {
    /// A reachability or business probe with only its primary address set.
    pub fn new(probe: impl Into<String>) -> Self {
        Self {
            probe: probe.into(),
            target: None,
            url: None,
            key: None,
            pointer: None,
            metric: None,
            degraded_above_ms: None,
            min: None,
            max: None,
            expect: None,
            worker_id: None,
            queue: None,
            login_env: None,
            token_env: None,
        }
    }
}

/// One observable resource in the deployment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopologyResource {
    /// Stable resource ID used across Snapshots, Issues, and Jobs.
    pub id: ResourceId,
    /// Resource kind; topology files may spell PostgreSQL as `postgresql`.
    pub kind: ResourceKind,
    /// ID of the hosting machine, when the resource is tied to one.
    #[serde(default)]
    pub node: Option<ResourceId>,
    /// Probes the Collector runs against this resource.
    #[serde(default)]
    pub probes: Vec<ProbeSpec>,
}

/// One dependency edge in the topology file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyDependency {
    /// Dependent resource ID.
    pub from: ResourceId,
    /// Depended-upon resource ID.
    pub to: ResourceId,
    /// Relationship name, such as `sql` or `redis_mq`.
    pub relation: String,
    /// Whether failure of the dependency blocks a core capability.
    #[serde(default)]
    pub critical: bool,
}

/// Complete static topology for one deployment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentTopology {
    /// Deployment identity and revision.
    pub deployment: DeploymentInfo,
    /// All observable resources.
    #[serde(default)]
    pub resources: Vec<TopologyResource>,
    /// Dependency edges between resources.
    #[serde(default)]
    pub dependencies: Vec<TopologyDependency>,
}

impl DeploymentTopology {
    /// Parses a topology from TOML text and validates its internal references.
    pub fn from_toml(text: &str) -> AgentResult<Self> {
        let topology: Self = toml::from_str(text)
            .map_err(|error| AgentError::InvalidInput(format!("topology parse failed: {error}")))?;
        topology.validate()?;
        Ok(topology)
    }

    /// Loads and validates a topology file from disk.
    pub fn load(path: &Path) -> AgentResult<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| AgentError::Io {
            context: format!("reading topology file `{}`", path.display()),
            source,
        })?;
        Self::from_toml(&text)
    }

    /// Converts the dependency entries into Snapshot dependency edges.
    pub fn dependency_edges(&self) -> Vec<DependencyEdge> {
        self.dependencies
            .iter()
            .map(|dep| DependencyEdge {
                from_resource_id: dep.from.clone(),
                to_resource_id: dep.to.clone(),
                relation: dep.relation.clone(),
                critical: dep.critical,
            })
            .collect()
    }

    /// Rejects duplicate resource IDs and dependency edges that reference unknown resources.
    ///
    /// A topology typo should fail at load time, in the operator's terminal, not mid-contest as a
    /// mysterious half-empty Snapshot.
    fn validate(&self) -> AgentResult<()> {
        let mut seen = std::collections::HashSet::new();
        for resource in &self.resources {
            if !seen.insert(&resource.id) {
                return Err(AgentError::InvalidInput(format!(
                    "topology declares resource `{}` more than once",
                    resource.id
                )));
            }
        }
        for dep in &self.dependencies {
            for endpoint in [&dep.from, &dep.to] {
                if !seen.contains(endpoint) {
                    return Err(AgentError::InvalidInput(format!(
                        "dependency `{}` -> `{}` references unknown resource `{endpoint}`",
                        dep.from, dep.to
                    )));
                }
            }
        }
        Ok(())
    }
}
