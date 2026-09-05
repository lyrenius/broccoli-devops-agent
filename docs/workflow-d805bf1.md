Implemented workflow for Broccoli DevOps Agent at commit `d805bf1`, inspected on 2026-09-05.

This diagram describes the currently wired report, action, inbox, and human-feedback paths. It includes the normal diagnosis result and the modeled Job/action failure outcomes. English labels are used so the diagram can be shared with Fable alongside the original design feedback.

```mermaid
flowchart TD
    Report["Human report<br/>CLI or Web API"] --> Capture["Collector captures and persists Snapshot"]
    Capture --> Issue["Create Issue"]
    Issue --> Dispatch["Build Snapshot View and create Job<br/>Report title + description + scope<br/>Issue: Investigating"]
    Dispatch --> Team["Run Operate Team<br/>Read-only rules or Harness model/tool loop"]

    Team -->|Diagnosis returned| Result["Persist JobResult<br/>Job: Completed / DiagnosisOnly"]
    Team -->|Job fails or Team backend errors| Failed["Failed Inbox<br/>Failed Jobs and failed actions<br/>Issue: WaitingForHuman"]
    Result --> Proposals{"Any action proposals?"}
    Proposals -->|No| Stop["End this pass<br/>No automatic Issue closure"]
    Proposals -->|Yes, for each proposal| Action["Capture before-Snapshot<br/>Create ActionRun"]
    Action --> Authority{"Authority matrix<br/>Operation mode + repeat escalation"}

    Authority -->|Auto| Platform["Agents Platform<br/>Validate and render runbook<br/>Dry-run or command execution<br/>Per-target execution lanes"]
    Authority -->|Approval required| Request["Permission Request Inbox<br/>Action: WaitingForApproval"]
    Request --> Decision{"Human decision"}
    Decision -->|Approve| Platform
    Decision -->|Reject with optional comment| Denial["Persist denial source, reason and comment<br/>Action: Cancelled<br/>Issue: WaitingForHuman"]
    Authority -->|Deny by policy| Denial
    Denial --> Denied["Permission Denied Inbox"]

    Platform -->|Failed execution result| Failed
    Platform -->|Successful execution result| Verify["Capture after-Snapshot and verify<br/>Observe: execution success is sufficient<br/>Other actions: every target must be Healthy"]
    Verify -->|Verification failed| Failed
    Verify -->|Verification passed| Success["Action: Succeeded<br/>Issue is not automatically resolved"]

    Denied --> Review{"Human review<br/>Reviewer + optional comment"}
    Failed --> Review
    Review -->|Acknowledge| Ack["Record review and remove item from Inbox<br/>No new Job; no automatic Issue closure"]
    Review -->|Send back upstream| Feedback["Append feedback to prior Job's feedback history<br/>Capture a fresh HumanFeedback Snapshot"]
    Feedback --> Revision["Build a new View with human_feedback<br/>Create a NEW Job under the SAME Issue<br/>Set revises_job_id; Issue: Investigating"]
    Revision --> Reviewed["Record review referencing the new Job<br/>Remove the old item from Inbox"]
    Reviewed --> Team

    classDef inbox fill:#eef4ff,stroke:#456aab,color:#172b4d;
    classDef feedback fill:#f4edff,stroke:#8050a6,color:#39264d;
    classDef finish fill:#f2f4f7,stroke:#667085,color:#344054;
    class Request,Denied,Failed inbox;
    class Feedback,Revision,Reviewed feedback;
    class Stop,Success,Ack finish;
```

Important details verified in the implementation:

- `WorkOrder` and the work-order policy decision point are gone. `JobBrief` only packages constructor inputs; its fields are stored on the Job. The View now contains the report title and description, scope, and human feedback.
- A Job is still one processing pass. Sending an item upstream creates a new Job under the same Issue, linked by `revises_job_id`; it does not resume the old completed or failed Job.
- The UI has three inbox categories. The Failed category combines `failed_jobs` and `failed_actions`; the latter includes both execution failure and verification failure. Inbox membership is computed from the stored records and whether they have a human review.
- Permission requests can be approved or rejected. Rejection records the human's identity and optional comment, then puts the action in Permission Denied. A rule denial records the policy rationale directly.
- Denied and failed items support `acknowledge` and `send_upstream`. Acknowledgement records a review without changing the old failure/denial into success or resolving its Issue.
- Sending upstream copies the selected prior Job's accumulated feedback and appends the new denial/failure information and reviewer comment. A fresh Snapshot and a revising Job are created, and the review naming that Job is persisted before its Team runs. Revised proposals pass through the same authority matrix.
- The Harness receives feedback in its input and in the stored View. The read-only Team includes the feedback in its diagnostic summary but still proposes no actions.
- The execution-layer scheduler is implemented as per-target locks in `LocalCommandPlatform`, within one Platform instance. Actual execution remains configured shell/runbook commands; there is no additional model-based execution Team in this block.
- Normal `DiagnosisOnly` completion, successful ActionRun verification, and inbox acknowledgement do not automatically resolve the Issue. A pending permission request also does not itself move the Issue to `WaitingForHuman`; denials and recorded Job/action failures do.
- Manual Snapshot capture remains a separate observation-only command. Periodic collection, automatic Snapshot Judge intake, and automatic next-step policy orchestration are not wired into this report/review path.

Code references:

- [Report dispatch and Team-error handling](../src/runner.rs#L269)
- [Action-proposal execution](../src/runner.rs#L323)
- [Human review and revision dispatch](../src/runner.rs#L391)
- [Inbox projection](../src/runner.rs#L548)
- [Snapshot View contents](../src/view.rs#L155)
- [Issue transitions after Team results](../src/scheduler.rs#L504)
- [Action verification](../src/scheduler.rs#L890)
- [Per-target execution lanes](../src/platform.rs#L72)
- [Inbox and feedback integration tests](../tests/inbox.rs)

Validation: `cargo test --workspace --all-targets --offline --quiet` passed all 57 tests, including the five inbox/feedback integration tests. Those integration tests use a scripted model, temporary stores, local probe listeners, and a dry-run Platform. `pnpm build` in `web/` also passed the TypeScript and Vite production build.
