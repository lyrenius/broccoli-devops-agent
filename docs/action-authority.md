# Action Authority Matrix (OD-2) — APPROVED

> Status: approved 2026-09-03; encoded in `src/policy.rs` (matrix and Runbook
> Registry) and enforced by the Scheduler at ActionRun creation  
> Scope: which Agents Platform operations may run automatically, which need a human's
> approval, and which are always denied — per operation mode.

This matrix is the policy the Scheduler applies when a Team proposes an action.
It is deliberately conservative: during a live contest the default answer is
"ask a human", and anything that could change verdicts, submission visibility,
or fairness is denied outright rather than left to judgment. Loosen it where the
rows below are stricter than your operations actually need.

## Legend

| Value | Meaning in code |
|---|---|
| **auto** | `ApprovalState::NotRequired` — the Scheduler moves the ActionRun straight to `Ready`; still evented, still before/after-verified |
| **approve** | `ApprovalState::Pending` — the ActionRun waits in the Permission Request inbox; a named human approves or rejects (with a comment) before anything executes |
| **deny** | `ApprovalState::Rejected` — the Scheduler refuses the proposal; the rationale stays on the ActionRun as its `denial`, and the item waits in the Permission Denied inbox, where a human can acknowledge it or send the reason back upstream to the Team as feedback |

Operation modes are the ones already in the domain model:
`rehearsal` (deployment and dry runs), `contest_locked` (a live contest), and
`post_contest` (archival and review).

## The matrix

| # | Operation class | Runbook examples | rehearsal | contest_locked | post_contest |
|---|---|---|---|---|---|
| 1 | **Observe** (probes, log reads, status queries) | `tcp.connect`, `http.status`, `service.status`, `log.tail` | auto | auto | auto |
| 2 | **Restart a judge worker** | `worker.restart` (graceful, drains current task) | auto | auto | auto |
| 3 | **Scale judge workers** (start an idle configured worker) | `worker.start` | auto | approve | auto |
| 4 | **Restart the API server** | `server.restart` (rolling if replicas > 1) | auto | approve | auto |
| 5 | **Restart the web frontend / gateway** | `frontend.restart`, `gateway.reload` | auto | approve | auto |
| 6 | **Restart a printer or balloon station** | `station.restart` | auto | auto | auto |
| 7 | **Requeue dead-letter jobs** (idempotent, no result change) | `mq.dlq_requeue` | auto | approve | auto |
| 8 | **Purge a queue** (drops pending work) | `mq.purge` | approve | deny | approve |
| 9 | **Change a tunable config key** (timeouts, pool sizes, worker concurrency) | `config.set` on the tunable allowlist | auto | approve | auto |
| 10 | **Change a contest-affecting config key** (scoring, time limits, visibility, freeze) | `config.set` on the contest allowlist | approve | deny | approve |
| 11 | **Change a security config key** (auth, CORS, secrets rotation) | `config.set` on the security allowlist | approve | deny | approve |
| 12 | **Firewall: allow a known internal address** | `ufw.allow` from the contest-LAN allowlist | auto | approve | auto |
| 13 | **Firewall: deny / remove a rule** | `ufw.deny`, `ufw.delete` | approve | approve | approve |
| 14 | **Deploy a plugin / WASM module** | `wasm.install` | approve | deny | approve |
| 15 | **Deploy a release Bundle** | `bundle.install` | approve | deny | approve |
| 16 | **Roll back to the previous Bundle / WASM** | `bundle.rollback`, `wasm.rollback` | auto | approve | auto |
| 17 | **Database: read-only diagnostic query** | `db.query_readonly` (allowlisted statements) | auto | auto | auto |
| 18 | **Database: maintenance** (VACUUM, REINDEX, ANALYZE) | `db.maintain` | auto | approve | auto |
| 19 | **Database: schema migration** | `db.migrate` | approve | deny | approve |
| 20 | **Database: any write to contest data** | — | deny | deny | approve |
| 21 | **Redis: flush or delete keys** | `redis.flush`, `redis.del` | approve | deny | approve |
| 22 | **Storage (CephFS/object store): delete or overwrite objects** | `storage.delete` | approve | deny | approve |
| 23 | **Storage: remount / restart storage daemon** | `storage.remount`, `ceph.restart_daemon` | approve | approve | approve |
| 24 | **Reboot a machine** | `machine.reboot` | approve | approve | approve |
| 25 | **Free-form shell command** | — | deny | deny | deny |
| 26 | **Change the operation mode itself** | `mode.set` | human-only | human-only | human-only |

## Rules that apply on top of the matrix

1. **Every side effect is an ActionRun.** Even `auto` rows are evented, carry an
   idempotency key, capture a before-Snapshot, and are verified against an
   after-Snapshot. `auto` means "no human gate", not "no record".
2. **`deny` cannot be overridden by approval.** If a denied operation is truly
   needed mid-contest, a human first switches the operation mode (row 26 — a
   human-only, logged action), which makes the trade-off explicit in the event
   log rather than buried in an approval click.
3. **A human can always deny.** Any `auto` row can be revoked by freezing
   dispatch (`dispatch_frozen`) or all side effects (`fully_frozen`).
4. **Allowlists are the unit of authority.** Rows 9–11 and 12 depend on config
   keys and addresses being classified in the Runbook Registry; an unclassified
   key is treated as contest-affecting (row 10) until someone classifies it.
5. **Rate limits on `auto` rows.** Suggested: at most one automatic restart of
   the same resource per 10 minutes; a second proposal within the window
   escalates to `approve`. This stops a flapping service from being restarted
   in a loop.
6. **Models never see this matrix as an instruction.** They see refusals as
   data. The matrix lives in the Scheduler and the Platform, not in prompts.

## Open questions for you

- Row 3 / 4 during a contest: is an automatic server restart acceptable if a
  replica exists and the restart is rolling? I left it at `approve`.
- Row 7: dead-letter requeue can cause a submission to be judged twice if a
  worker did finish it; the Broccoli worker is idempotent on verdicts, so I
  marked rehearsal `auto` — confirm this holds for your deployment.
- Row 12: which addresses belong on the contest-LAN allowlist (station subnet,
  judge subnet, printer IPs)?
- Row 16: automatic rollback in rehearsal assumes the previous Bundle is
  retained by the Platform. That retention rule belongs to OD-7.
- Anything printer-specific you want as `auto` during a contest (e.g. clearing
  a stuck print job) — I have no row for it yet.

The open questions above remain open; the matrix is enforced as written until
they are answered. Changing a row means changing `OperationClass::authority`
in `src/policy.rs` and the corresponding test.
