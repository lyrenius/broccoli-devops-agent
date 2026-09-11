# Versioned operational runbooks

These adapters implement the configured five-VPS deployment. They do not provision
credentials or change the current agent configuration merely by being present.

- Controller: `runbook.sh` maps known resource IDs to the existing internal hosts.
  It uses the dedicated restricted SSH identity. `app-health.py` reads the existing
  application health endpoints locally from the controller.
- Nodes: `remote-dispatch.sh` is the root-owned forced-command dispatcher. The
  deployment role in `/etc/broccoli-node-role` constrains targets and verbs.
- Infra helpers: `redis-health.py` and `infra-ops.py` are installed under
  `/usr/local/lib/broccoli/`. They read the node's existing configuration; fixed
  PostgreSQL queries run with a read-only session and statement timeout.
- `redact-output.py` removes configured credentials before logs leave a node.

The registry and authority matrix remain in `src/policy.rs`; executable mappings
are supplied by the deployment's `[platform.runbooks]` configuration. A missing
mapping or unregistered operation is returned for human intervention. Deploy the
matching adapters before enabling their mappings, and preserve the current
configuration's operation mode and approval requirements.

Example mappings use `/opt/broccoli-agent/cloud/runbook.sh <verb> {target}`:

| Runbook | Verb |
|---|---|
| redis.ping | redis-ping |
| redis.info | redis-info |
| postgres.check | postgres-check |
| postgres.locks | postgres-locks |
| storage.check | storage-check |
| infra.resources | resources |
| app.health | app-health |
| redis.start / postgres.start / storage.start | start |
| redis.restart / postgres.restart / storage.restart | restart |

Diagnostic success is an observation, not evidence of a repair. A responsive S3
endpoint (including HTTP 403) does not prove object read/write access. Restarts
require after-Snapshot verification; storage restarts retain human approval.

Local adapter checks: `python3 cloud/experiments/test_operational_helpers.py`.
