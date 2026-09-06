//! Every screen rendered over one fixture deployment, and the keys that need no server.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

use crate::api::{
    ActionRun, ApiClient, EventRecord, Inbox, Issue, Job, SessionBundle, SettingsPage, Snapshot,
    Status,
};
use crate::app::{App, Msg, Pending, Screen};
use crate::screens::inbox::{Category, items};
use crate::screens::trace::build_rows;
use crate::ui;

const ISSUE: &str = "0198c936-5f2a-7000-8000-4a6f8c2d9dd1";
const JOB_DONE: &str = "0198c936-5f2a-7000-8000-4a6f8c2d9dd2";
const JOB_LIVE: &str = "0198c936-5f2a-7000-8000-4a6f8c2d9dd3";
const ACTION: &str = "0198c936-5f2a-7000-8000-4a6f8c2d9dd4";
const NOW: &str = "2026-09-06T10:12:03Z";

fn app() -> (App, mpsc::UnboundedReceiver<Msg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        ApiClient::new("http://127.0.0.1:9", None),
        "ana".into(),
        "http://127.0.0.1:9".into(),
        tx,
    );
    app.now = crate::format::parse_time(NOW).unwrap();
    (app, rx)
}

fn action(id: &str, runbook: &str, status: &str, approval: &str) -> ActionRun {
    serde_json::from_value(json!({
        "action_run_id": id,
        "issue_id": ISSUE,
        "originating_job_id": JOB_DONE,
        "runbook_id": runbook,
        "target_ids": ["redis-mq"],
        "reason": "queue is stuck",
        "expected_effect": "judging resumes",
        "status": status,
        "approval": approval,
        "approved_by": null,
        "denial": null,
        "review": null,
        "execution_summary": null,
        "dry_run": true,
        "verification_summary": null,
        "verification_evidence": null,
        "created_at": NOW
    }))
    .unwrap()
}

fn job(id: &str, status: &str, result: Option<serde_json::Value>) -> Job {
    serde_json::from_value(json!({
        "job_id": id,
        "issue_id": ISSUE,
        "team_kind": "operate",
        "status": status,
        "created_at": "2026-09-06T10:11:00Z",
        "snapshot_view": { "snapshot_id": "s", "artifact_id": "view-artifact", "content_sha256": "x" },
        "usage": { "model": "m", "input_tokens": 9800, "cached_input_tokens": 6000, "output_tokens": 420, "requests": 2, "requests_without_usage": 0 },
        "feedback": [],
        "revises_job_id": null,
        "review": null,
        "result": result
    }))
    .unwrap()
}

/// A deployment mid-incident: one request, one denial, one failed Job, a live pass.
fn populated() -> (App, mpsc::UnboundedReceiver<Msg>) {
    let (mut app, rx) = app();
    let status: Status = serde_json::from_value(json!({
        "mode": "dispatch_frozen",
        "team_backend": "scripted live demo",
        "dry_run": true,
        "uptime_secs": 3720,
        "deployment": { "name": "live-demo", "topology_revision": "demo-1", "operation_mode": "rehearsal" },
        "language": "en",
        "snapshot_interval_secs": 120,
        "recovery": { "previous_mode": "running", "final_mode": "dispatch_frozen", "interrupted_job_ids": ["j"], "interrupted_action_ids": [], "pending_recovery_review": true },
        "counts": { "issues": 1, "jobs": 2, "actions": 3, "events": 42 },
        "inbox": { "permission_requests": 1, "permission_denied": 1, "failed_jobs": 1, "failed_actions": 0, "total": 3 },
        "running": [{ "job_id": JOB_LIVE, "issue_id": ISSUE, "started_at": "2026-09-06T10:11:30Z" }],
        "usage": { "passes": 3, "input_tokens": 29400, "cached_input_tokens": 18000, "output_tokens": 1260, "total_tokens": 30660, "requests": 6, "requests_without_usage": 0, "cost": 0.0123, "currency": "USD", "by_model": [], "budget": { "max_total_tokens": 100000, "max_total_cost": 0.0, "exceeded": false, "reason": null, "used_fraction": 0.31 } }
    }))
    .unwrap();
    app.status = Some(status);
    let snapshot: Snapshot = serde_json::from_value(json!({
        "snapshot_id": "s1", "created_at": "2026-09-06T10:11:50Z", "cause": "manual", "operation_mode": "rehearsal", "topology_revision": "demo-1",
        "resources": [
            { "resource_id": "redis-mq", "kind": "redis", "health": "down", "observed_at": NOW, "metrics": [{ "name": "probe.tcp.latency", "value": 12.0, "unit": "ms" }], "facts": [] },
            { "resource_id": "worker-1", "kind": "worker", "health": "healthy", "observed_at": NOW, "metrics": [{ "name": "worker.in_flight", "value": 3.0, "unit": "" }], "facts": [] }
        ],
        "coverage_gaps": [{ "resource_id": "worker-1", "probe_id": "redis.llen", "reason": "no redis endpoint" }]
    }))
    .unwrap();
    app.snapshot = Some(snapshot);
    let mut denied = action(ACTION, "mode.set", "cancelled", "rejected");
    denied.denial = Some(serde_json::from_value(json!({ "source": "policy", "reason": "row 26 is human-only", "comment": null, "decided_by": null, "decided_at": NOW })).unwrap());
    let request = action(
        "0198c936-5f2a-7000-8000-4a6f8c2d9dd5",
        "mq.purge",
        "waiting_for_approval",
        "pending",
    );
    let failed_job = job(
        "0198c936-5f2a-7000-8000-4a6f8c2d9dd6",
        "failed",
        Some(json!({
            "outcome": "failed", "summary": "the model ended without calling submit_diagnosis", "unresolved_questions": [], "proposed_actions": [], "artifact_ids": []
        })),
    );
    app.inbox = Inbox {
        permission_requests: vec![request.clone()],
        permission_denied: vec![denied.clone()],
        failed_jobs: vec![failed_job],
        failed_actions: vec![],
    };
    let mut decided = action(
        "0198c936-5f2a-7000-8000-4a6f8c2d9dd7",
        "svc.restart",
        "succeeded",
        "approved",
    );
    decided.approved_by = Some("ana".into());
    decided.verification_summary = Some("worker-1 healthy after the restart".into());
    decided.verification_evidence = Some("strong".into());
    app.actions = vec![request, denied, decided];
    let issue: Issue = serde_json::from_value(json!({
        "issue_id": ISSUE, "source": "human", "title": "Contestants cannot submit", "description": "Web submissions time out since 10:12",
        "priority": "human_top", "status": "investigating", "created_at": "2026-09-06T10:10:00Z", "updated_at": NOW, "affected_resource_ids": ["redis-mq"], "provenance": null
    }))
    .unwrap();
    app.issues = vec![issue.clone()];
    let done = job(
        JOB_DONE,
        "completed",
        Some(json!({
            "outcome": "diagnosis_only", "summary": "redis-mq is unreachable, so worker-1 idles", "unresolved_questions": ["Is the Redis host reachable?"],
            "proposed_actions": [{ "runbook_id": "mq.purge", "target_ids": ["redis-mq"], "reason": "stale", "expected_effect": "drains" }], "artifact_ids": ["transcript-artifact"]
        })),
    );
    let mut live = job(JOB_LIVE, "running", None);
    live.continues_job_id = Some(JOB_DONE.into());
    live.usage = None;
    app.jobs = vec![done.clone(), live.clone()];
    app.events = (1..=3)
        .map(|i| EventRecord {
            sequence: i,
            occurred_at: NOW.into(),
            actor: if i == 2 {
                "agent-team".into()
            } else {
                "human".into()
            },
            kind: if i == 2 {
                "team.callback".into()
            } else {
                "scheduler.issue_created".into()
            },
            summary: format!("event number {i}"),
            issue_id: Some(ISSUE.into()),
            job_id: Some(JOB_LIVE.into()),
            ..EventRecord::default()
        })
        .collect();
    app.events_seq = 3;
    app.events_connected = true;

    let bundle: SessionBundle = serde_json::from_value(json!({
        "deployment": "live-demo",
        "issue": serde_json::to_value(json!({
            "issue_id": ISSUE, "source": "human", "title": "Contestants cannot submit", "description": "Web submissions time out since 10:12",
            "priority": "human_top", "status": "investigating", "created_at": "2026-09-06T10:10:00Z", "updated_at": NOW, "affected_resource_ids": ["redis-mq"], "provenance": null
        })).unwrap(),
        "jobs": [
            json!({ "job_id": JOB_DONE, "issue_id": ISSUE, "team_kind": "operate", "status": "completed", "created_at": "2026-09-06T10:11:00Z",
                "snapshot_view": { "snapshot_id": "s", "artifact_id": "view-artifact" },
                "usage": { "model": "m", "input_tokens": 9800, "cached_input_tokens": 6000, "output_tokens": 420, "requests": 2, "requests_without_usage": 0 },
                "result": { "outcome": "diagnosis_only", "summary": "redis-mq is unreachable, so worker-1 idles", "unresolved_questions": ["Is the Redis host reachable?"], "proposed_actions": [], "artifact_ids": ["transcript-artifact"] } }),
            json!({ "job_id": JOB_LIVE, "issue_id": ISSUE, "team_kind": "operate", "status": "running", "created_at": "2026-09-06T10:11:30Z", "continues_job_id": JOB_DONE,
                "snapshot_view": { "snapshot_id": "s", "artifact_id": "view-artifact" }, "result": null })
        ],
        "action_runs": [],
        "artifacts": [
            { "artifact": { "artifact_id": "view-artifact", "kind": "snapshot_view", "size_bytes": 10 }, "body": { "encoding": "json", "content": { "resources": ["redis-mq"] } } },
            { "artifact": { "artifact_id": "transcript-artifact", "kind": "diagnostic_bundle", "size_bytes": 10 }, "body": { "encoding": "json", "content": {
                "instructions": "Diagnose the deployment.",
                "entries": [
                    { "at": "2026-09-06T10:11:01Z", "item": { "type": "user_input", "text": "View: redis-mq down", "trust": "untrusted" } },
                    { "at": "2026-09-06T10:11:03Z", "item": { "type": "tool_call", "call_id": "c1", "tool": "read_snapshot_view", "arguments": {} } },
                    { "at": "2026-09-06T10:11:04Z", "item": { "type": "tool_output", "call_id": "c1", "tool": "read_snapshot_view", "output": { "resources": 2 }, "is_error": false, "trust": "untrusted" } },
                    { "at": "2026-09-06T10:11:06Z", "item": { "type": "assistant_text", "text": "redis-mq is down and worker-1 with it." } }
                ],
                "turns": [{ "turn": 1, "started_at": "2026-09-06T10:11:00Z", "finished_at": "2026-09-06T10:11:03Z", "first_entry": 1, "usage": { "input_tokens": 9800, "cached_input_tokens": 6000, "output_tokens": 420, "requests": 1, "requests_without_usage": 0 }, "retries": 1, "wrap_up": false, "offered_tools": ["read_snapshot_view"] }]
            } } }
        ],
        "events": [{ "sequence": 1, "occurred_at": NOW, "actor": "scheduler-policy", "kind": "scheduler.job_dispatched", "summary": "dispatched", "issue_id": ISSUE, "job_id": JOB_DONE }]
    }))
    .unwrap();
    // ActionRun has no Serialize; the bundle's actions are set by hand.
    let mut bundle = bundle;
    bundle.action_runs = vec![action(
        ACTION,
        "mq.purge",
        "waiting_for_approval",
        "pending",
    )];
    app.trace = crate::screens::trace::TraceState::new(ISSUE.into());
    app.trace.bundle = Some(bundle);
    app.trace.steps.insert(
        JOB_LIVE.into(),
        vec![serde_json::from_value(json!({ "index": 0, "at": "2026-09-06T10:11:31Z", "item": { "type": "assistant_text", "text": "Correlating the Redis outage" }, "truncated": true })).unwrap()],
    );
    let page: SettingsPage = serde_json::from_value(json!({
        "config": {
            "agent": { "language": "en", "max_auto_passes": 3 },
            "data": { "dir": "data-live" },
            "collector": { "snapshot_interval_secs": 120 },
            "topology": { "path": "data-live/topology.toml" },
            "model": null,
            "platform": { "dry_run": true, "command_timeout_secs": 30, "auto_repeat_window_secs": 600,
                "classification": { "tunable_config_keys": ["a"], "contest_config_keys": [], "security_config_keys": [], "known_internal_address_prefixes": ["10."] },
                "runbooks": [{ "id": "mq.purge", "command": "echo purge {target}" }] },
            "api": { "bind": "127.0.0.1:4720", "token": "" },
            "budget": { "max_total_tokens": 100000, "max_total_cost": 0.0 }
        },
        "path": "data-live/agent.toml",
        "mode": "dispatch_frozen",
        "classes": { "live": ["collector.snapshot_interval_secs", "agent.max_auto_passes", "budget.max_total_tokens", "budget.max_total_cost"], "policy": ["platform.dry_run"], "startup": ["agent.language"] }
    }))
    .unwrap();
    app.settings.draft = Some(page.config.clone());
    app.settings.page = Some(page);
    (app, rx)
}

fn render(app: &mut App) -> String {
    let backend = TestBackend::new(170, 52);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn press(app: &mut App, code: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn every_screen_renders_the_fixture() {
    let (mut app, _rx) = populated();
    let overview = render(&mut app);
    assert!(overview.contains("dispatch frozen"), "{overview}");
    assert!(overview.contains("live-demo (rehearsal)"));
    assert!(overview.contains("Recovered from a restart"));
    assert!(overview.contains("1 / 2"), "healthy tile");
    assert!(overview.contains("0.0123 USD"));
    assert!(overview.contains("budget 31% used"));
    assert!(overview.contains("redis-mq"));
    assert!(overview.contains("Judge worker"));
    assert!(overview.contains("no redis endpoint"), "coverage gap");
    assert!(
        overview.contains("↳ 10:12:03 event number 2") || overview.contains("event number 2"),
        "latest callback"
    );

    app.set_screen(Screen::Inbox);
    let inbox = render(&mut app);
    assert!(inbox.contains("mq.purge on redis-mq"));
    assert!(inbox.contains("needs approval"));
    assert!(inbox.contains("denied by rule"));
    assert!(inbox.contains("job failed"));
    assert!(
        inbox.contains("Expected effect: judging resumes"),
        "{inbox}"
    );
    press(&mut app, KeyCode::Char('h'));
    let history = render(&mut app);
    assert!(history.contains("svc.restart on redis-mq"));
    assert!(history.contains("approved by ana"));
    assert!(history.contains("strong evidence"));
    press(&mut app, KeyCode::Char('h'));

    app.set_screen(Screen::Records);
    let records = render(&mut app);
    assert!(records.contains("Contestants cannot submit"));
    assert!(records.contains("human top"));
    assert!(records.contains("redis-mq is unreachable, so worker-1 idles"));
    assert!(records.contains("follows 0198c936"), "{records}");
    assert!(records.contains("? Is the Redis host reachable?"));

    app.set_screen(Screen::Trace);
    let trace = render(&mut app);
    assert!(trace.contains("Trace: Contestants cannot submit"));
    assert!(trace.contains("Pass 1"));
    assert!(trace.contains("Pass 2"));
    assert!(
        trace.contains("Waiting for the first entry")
            || trace.contains("Correlating the Redis outage"),
        "{trace}"
    );
    press(&mut app, KeyCode::Left);
    let stored = render(&mut app);
    assert!(stored.contains("Turn 1"), "{stored}");
    assert!(stored.contains("read_snapshot_view"));
    assert!(stored.contains("untrusted data"));
    assert!(stored.contains("1 retry after a backend failure"));
    assert!(stored.contains("Result"));
    assert!(stored.contains("Actions proposed by this pass"));
    assert!(stored.contains("mq.purge on redis-mq"));
    press(&mut app, KeyCode::Char('I'));
    press(&mut app, KeyCode::Char('v'));
    let folds = render(&mut app);
    assert!(folds.contains("Diagnose the deployment."));
    assert!(folds.contains("\"resources\""));

    app.set_screen(Screen::Events);
    let events = render(&mut app);
    assert!(events.contains("scheduler.issue_created"));
    assert!(events.contains("team.callback"));
    assert!(events.contains("live"));

    app.set_screen(Screen::Report);
    let report = render(&mut app);
    assert!(report.contains("What you observed"));
    assert!(report.contains("File report"));

    app.set_screen(Screen::Settings);
    let settings = render(&mut app);
    assert!(settings.contains("Snapshot every (seconds)"));
    assert!(settings.contains("120"));
    assert!(settings.contains("Runbook 1 command"));
    assert!(settings.contains("echo purge {target}"));
    assert!(settings.contains("data-live/agent.toml"));

    press(&mut app, KeyCode::Char('?'));
    let help = render(&mut app);
    assert!(help.contains("Keys"));
    assert!(help.contains("send the selected denial"), "{help}");
}

#[test]
fn inbox_decisions_apply_only_where_they_belong() {
    let (mut app, _rx) = populated();
    app.set_screen(Screen::Inbox);
    let rows = items(&app);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].category, Category::Request);
    // Send-upstream does not apply to a request: no prompt opens, a message explains.
    press(&mut app, KeyCode::Char('b'));
    assert!(app.prompt.is_none());
    assert!(
        app.message
            .as_deref()
            .unwrap()
            .contains("does not apply to a request")
    );
    // Reject does: the prompt collects the comment and Esc cancels it.
    press(&mut app, KeyCode::Char('r'));
    assert!(matches!(
        app.prompt.as_ref().unwrap().pending,
        Pending::Reject { .. }
    ));
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('o'));
    assert_eq!(app.prompt.as_ref().unwrap().buffer, "no");
    press(&mut app, KeyCode::Esc);
    assert!(app.prompt.is_none());
    // The filter narrows the rows and the selection follows.
    press(&mut app, KeyCode::Char(']'));
    assert_eq!(items(&app).len(), 1);
    press(&mut app, KeyCode::Char(']'));
    assert_eq!(items(&app)[0].category, Category::Denied);
    press(&mut app, KeyCode::Char('x'));
    assert!(matches!(
        app.prompt.as_ref().unwrap().pending,
        Pending::Review {
            decision: "acknowledge",
            ..
        }
    ));
}

#[test]
fn records_search_and_close_prompts() {
    let (mut app, _rx) = populated();
    app.set_screen(Screen::Records);
    press(&mut app, KeyCode::Char('/'));
    for c in "nothing-here".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);
    assert!(crate::screens::records::visible(&app).is_empty());
    press(&mut app, KeyCode::Char('/'));
    for _ in 0..12 {
        press(&mut app, KeyCode::Backspace);
    }
    press(&mut app, KeyCode::Enter);
    assert_eq!(crate::screens::records::visible(&app).len(), 1);
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(
        app.prompt.as_ref().unwrap().pending,
        Pending::CloseIssue {
            outcome: "resolved",
            ..
        }
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('e'));
    assert_eq!(app.prompt.as_ref().unwrap().buffer, "session-0198c936.json");
    press(&mut app, KeyCode::Esc);
    // Enter opens the trace of the selected Issue.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Trace);
    assert_eq!(app.trace.issue_id.as_deref(), Some(ISSUE));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen, Screen::Records);
}

#[test]
fn settings_edits_respect_the_classes() {
    let (mut app, _rx) = populated();
    app.set_screen(Screen::Settings);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    let prompt = app
        .prompt
        .as_ref()
        .expect("the first live setting opens a prompt");
    assert!(
        matches!(&prompt.pending, Pending::Setting { key } if key == "collector.snapshot_interval_secs")
    );
    assert_eq!(prompt.buffer, "120");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Char('5'));
    press(&mut app, KeyCode::Enter);
    let draft = app.settings.draft.as_ref().unwrap();
    assert_eq!(draft["collector"]["snapshot_interval_secs"], 125);
    // The Scheduler is frozen in the fixture, so dry-run toggles; unfreeze it and it locks.
    let dry_run = crate::screens::settings::rows(
        draft,
        app.settings.page.as_ref().unwrap(),
        "dispatch_frozen",
    )
    .iter()
    .position(|r| r.key == "platform.dry_run")
    .unwrap();
    app.settings.list.select(Some(dry_run));
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.settings.draft.as_ref().unwrap()["platform"]["dry_run"],
        false
    );
    app.status.as_mut().unwrap().mode = "running".into();
    press(&mut app, KeyCode::Enter);
    assert!(
        app.message
            .as_deref()
            .unwrap()
            .contains("only while the Scheduler is frozen")
    );
    // Saving with dry-run turned off asks for the confirmation before anything is sent.
    app.status.as_mut().unwrap().mode = "dispatch_frozen".into();
    press(&mut app, KeyCode::Char('w'));
    assert!(matches!(
        app.prompt.as_ref().unwrap().pending,
        Pending::ConfirmLive
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('U'));
    assert_eq!(
        app.settings.draft.as_ref().unwrap()["collector"]["snapshot_interval_secs"],
        120
    );
}

#[test]
fn report_form_takes_the_keys_until_esc() {
    let (mut app, _rx) = populated();
    app.set_screen(Screen::Report);
    press(&mut app, KeyCode::Enter);
    for c in "Queue".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Tab);
    for c in "stuck".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('q'));
    assert_eq!(app.report.title, "Queue");
    assert_eq!(app.report.description, "stuck\nq");
    assert_eq!(
        app.screen,
        Screen::Report,
        "q inside the form is text, not quit"
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.report.focus.is_none());
    assert!(
        !press(&mut app, KeyCode::Char('q')),
        "q outside the form quits"
    );
}

#[test]
fn transcript_rows_pair_calls_with_outputs_and_time_them() {
    let entries: Vec<crate::api::TranscriptEntry> = serde_json::from_value(json!([
        { "at": "2026-09-06T10:11:01.000Z", "item": { "type": "user_input", "text": "hi", "trust": "trusted" } },
        { "at": "2026-09-06T10:11:03.000Z", "item": { "type": "tool_call", "call_id": "c1", "tool": "t", "arguments": {} } },
        { "at": "2026-09-06T10:11:04.500Z", "item": { "type": "tool_output", "call_id": "c1", "tool": "t", "output": "ok", "is_error": false, "trust": "untrusted" } },
        { "at": "2026-09-06T10:11:06.000Z", "item": { "type": "assistant_text", "text": "done" } }
    ]))
    .unwrap();
    let end = crate::format::millis("2026-09-06T10:11:10Z");
    let rows = build_rows(&entries, &[], end, 0);
    assert_eq!(rows.len(), 3, "the output folds into its call");
    assert_eq!(rows[0].duration, 2000);
    assert_eq!(rows[1].duration, 1500, "a tool row spans call to output");
    assert_eq!(rows[1].output.as_ref().unwrap().0, 2);
    assert_eq!(
        rows[2].duration, 4000,
        "the last row runs to the end of the pass"
    );
}

#[test]
fn events_tail_follows_the_newest_and_the_trace_takes_steps() {
    let (mut app, _rx) = populated();
    app.handle_msg(Msg::Events(Ok(vec![EventRecord {
        sequence: 9,
        kind: "team.callback".into(),
        summary: "later".into(),
        ..EventRecord::default()
    }])));
    assert_eq!(app.events_seq, 9);
    assert_eq!(
        app.events_view.list.selected(),
        Some(3),
        "following selects the newest"
    );
    let step_event: EventRecord = serde_json::from_value(json!({
        "sequence": 10, "occurred_at": NOW, "actor": "agent-team", "kind": "team.step", "summary": "s", "issue_id": ISSUE, "job_id": JOB_LIVE,
        "payload": { "step": { "index": 1, "at": NOW, "item": { "type": "tool_call", "call_id": "c9", "tool": "report_progress", "arguments": {} }, "truncated": false } }
    }))
    .unwrap();
    let other: EventRecord = serde_json::from_value(json!({
        "sequence": 11, "occurred_at": NOW, "actor": "scheduler-policy", "kind": "scheduler.job_continued", "summary": "continued", "issue_id": ISSUE, "job_id": JOB_LIVE
    }))
    .unwrap();
    app.handle_msg(Msg::SessionEvents {
        issue_id: ISSUE.into(),
        events: vec![step_event, other],
    });
    assert_eq!(
        app.trace.steps[JOB_LIVE].len(),
        2,
        "the step joined the live transcript"
    );
    assert!(app.trace.dirty, "any other event re-reads the session");
    assert_eq!(app.trace.last_seq, 11);
}
