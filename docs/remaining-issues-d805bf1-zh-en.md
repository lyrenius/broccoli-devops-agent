基于提交 `d805bf1` 的剩余问题汇总，检查日期：2026-09-05。

Remaining issues at commit `d805bf1`, inspected on 2026-09-05.

本轮已经落实了去掉 WorkOrder、三类 Inbox、拒绝原因与人工反馈进入下一轮，以及 Team 异常转为 Failed Job。以下将实现缺口、设计选择和验证边界分开，不把所有未实现功能都视为 bug。

The latest change implements WorkOrder removal, the three inbox categories, denial/comment feedback in revision passes, and conversion of Team errors into failed Jobs. The findings below distinguish implementation gaps, design choices, and validation limits.

1. **高优先级：权限没有绑定完整的操作范围 / High priority: authorization does not validate the complete operation scope.**

   中文：权限矩阵按 runbook 和参数分类，Platform 主要确认目标 ID 在拓扑中，没有将 runbook、资源类型和 Job 的授权范围联合校验。例如 `worker.restart` 在比赛模式下可自动执行，而 `server.restart` 需要审批；当前允许把前者指向一个已知的非 worker 资源。本地 testbed 的通用 restart 映射会按目标 ID 选择容器，因此目标类型错误会影响真实操作，而不只是名称不准确。

   English: The authority matrix classifies runbooks and arguments, while the Platform mainly checks whether a target ID exists in the topology. It does not jointly validate the runbook, resource type, and Job scope. For example, `worker.restart` is automatic during a contest while `server.restart` requires approval, yet a known non-worker target is not rejected for `worker.restart`. The testbed's generic restart mapping selects the actual container by target ID, making this an execution authorization gap.

   修复方向 / Fix direction: 对操作、目标类型、Job 范围和参数进行统一校验。Validate the operation, target type, Job scope, and arguments together before granting and exercising authority.

   Evidence: [matrix](../src/policy.rs#L136), [ActionRun creation](../src/scheduler.rs#L625), [Platform validation](../src/platform.rs#L193), [testbed mapping](../testbed/runbook.sh#L35).

2. **高优先级：幂等与并发领取不完整 / High priority: idempotency and atomic execution claims are missing.**

   中文：`idempotency_key` 被生成并保存，但执行端没有使用它去重。目标锁只保证命令排队，不保证同一操作只执行一次；审批、启动和审核采用分开的读取与更新，也没有原子状态领取。并发请求或重试可能重复执行动作；送回上游又是先创建 Job、后写审核记录，也存在重复派发或遗留 Job 的窗口。

   English: An `idempotency_key` is generated and stored but is not used to deduplicate execution. Per-target locks serialize commands; they do not guarantee a single execution of a logical operation. Approval, start, and review use separate reads and updates without atomic claims. Concurrent requests or retries can duplicate execution. Sending an item upstream also creates a Job before recording its review, leaving a window for duplicate dispatch or an orphaned Job.

   修复方向 / Fix direction: 持久化请求标识与执行领取状态，并原子地处理审核和派发。Persist idempotency/claim state and make review-plus-dispatch and execution claims atomic.

   Evidence: [proposal handling](../src/runner.rs#L323), [review sequence](../src/runner.rs#L499), [approval](../src/scheduler.rs#L725), [execution start](../src/scheduler.rs#L825), [target locks](../src/platform.rs#L72).

3. **高优先级：超时没有终止子进程 / High priority: a timeout does not terminate the child process.**

   中文：Platform 对 `Command::output()` 外层设置 timeout，但没有显式终止和回收子进程。使用本地 Tokio 1.53.1 编译库的最小复现得到 `wait_timed_out=true` 和 `child_wrote_after_timeout=true`：等待超时后，子进程仍继续执行并写入临时文件。因此动作可能已显示失败、目标锁已经释放，但原命令仍在运行。

   English: The Platform wraps `Command::output()` in a timeout without explicitly terminating and reaping the child process. An isolated reproducer using the locally built Tokio 1.53.1 library returned `wait_timed_out=true` and `child_wrote_after_timeout=true`: the child continued and wrote a temporary file after the wait timed out. An action can therefore be reported as failed and release its target lock while the original command is still running.

   修复方向 / Fix direction: 显式管理子进程及必要的进程组，确认终止后再结束操作和释放锁。Manage the child/process group explicitly and confirm termination before completing the operation and releasing its lock.

   Evidence: [timeout and subprocess call](../src/platform.rs#L253). The reproducer exercised the same timeout/output pattern with only a temporary marker file; it did not execute testbed runbooks.

4. **高优先级：重启恢复与持久化不完整 / High priority: restart recovery and durable state transitions are incomplete.**

   中文：`serve` 启动没有调用恢复流程，新的 Scheduler 直接从 Running 开始，之前的冻结状态不会自动恢复。显式 `recover()` 只枚举未完成对象，不会恢复执行或核实中断动作的实际结果。文件直接覆写，多对象更新没有事务，崩溃时可能留下部分写入或状态不一致。

   English: `serve` does not invoke recovery, and a new Scheduler starts in Running mode, so a previous freeze is not automatically restored. Explicit recovery only enumerates unfinished records; it does not resume execution or reconcile the actual outcome of interrupted actions. Files are overwritten directly and multi-object updates are not transactional, leaving partial-write and inconsistent-state failure modes.

   修复方向 / Fix direction: 启动先恢复控制状态和冻结状态，核实中断任务，并保证关键写入的原子性。Recover control/freeze state at startup, reconcile interrupted work, and make critical persistence transitions atomic.

   Evidence: [serve startup](../src/main.rs#L417), [Scheduler constructor](../src/scheduler.rs#L95), [recovery](../src/scheduler.rs#L1049), [file writes](../src/store/file.rs#L129).

5. **工作流缺口：Issue 缺少完整结案与状态汇总 / Workflow gap: Issue closure and aggregate status handling are incomplete.**

   中文：正常诊断返回 DiagnosisOnly 后 Issue 保持 Investigating；ActionRun 成功也不会推进 Issue 结案。Acknowledge 只清除 Inbox 待办本身是合理的，但系统缺少完整的 Issue 级验证、人工关闭或取消入口，以及根据剩余 Job、审批和审核事项汇总状态的规则。因此可能没有活跃任务和 Inbox 待办，Issue 却仍显示处理中或等待人工。

   English: Normal DiagnosisOnly completion leaves the Issue Investigating, and successful ActionRuns do not advance Issue closure. Acknowledgement appropriately clears only an inbox item, but the workflow lacks complete Issue-level verification, explicit close/cancel entry points, and status reconciliation across remaining Jobs, approvals, and reviews. An Issue can remain Investigating or WaitingForHuman even when there is no active work or inbox item to handle.

   修复方向 / Fix direction: 明确 Issue 的解决条件和人工关闭/取消操作，并汇总其子任务与待办状态；不要把 Acknowledge 直接等同于解决。Define Issue resolution criteria and explicit close/cancel operations, reconcile child-work states, and keep acknowledgement distinct from resolution.

   Evidence: [callback mapping](../src/scheduler.rs#L504), [acknowledgement](../src/runner.rs#L490), [verification result](../src/scheduler.rs#L947), [API routes](../src/api.rs#L96).

6. **验证缺口：Healthy 不等于预期效果达成 / Verification gap: Healthy does not establish the expected effect.**

   中文：修改类动作统一按 after-Snapshot 中目标是否 Healthy 判断，尚未按 `expected_effect` 或各动作的验证探针检查具体效果。目标原本就健康时可能通过验证；没有探针的 worker 即使修复也仍是 Unknown。当前观测还缺 worker 心跳、队列积压、判题结果等业务证据，延迟上升也没有相应健康阈值。

   English: Mutating actions are verified through the generic condition that all targets are Healthy in the after-Snapshot, rather than their specific expected effects or verification probes. An already healthy target can pass, while a repaired but unprobed worker remains Unknown. Observation also lacks worker heartbeats, queue backlog, judging results, and latency-based health criteria.

   修复方向 / Fix direction: 为各类动作定义可观测的后置条件，补充业务探针，并区分演练结果与真实修复证据。Define observable, operation-specific postconditions, add business-level probes, and distinguish dry-run outcomes from live remediation evidence.

   Evidence: [verification](../src/scheduler.rs#L883), [collector](../src/collector.rs#L61), [testbed worker coverage](../config/topology.testbed.toml#L57).

7. **失败处理缺口：异常收尾和证据链不完整 / Failure-handling gap: exception finalization and execution evidence are incomplete.**

   中文：Team 异常已能形成 Failed Job，但 Platform 或 after-Snapshot 阶段返回的错误仍可能直接向上传播，把 ActionRun 留在 Running/Verifying，无法进入只筛选明确失败状态的 Inbox。另外，ActionOutput 正文写入磁盘后没有把 Artifact 元数据注册到 StateStore，按返回的 ID 通过 Artifact API 查询会缺失；送回上游的执行失败反馈还可能只有 `execution ended as Failed`，缺少实际 stderr、退出码和执行证据。

   English: Team errors now become failed Jobs, but errors returned by the Platform or after-Snapshot stage can still propagate while leaving an ActionRun Running or Verifying, outside the failure inbox. ActionOutput bodies are also written without registering their Artifact metadata in StateStore, so the Artifact API cannot resolve their returned IDs. Upstream execution-failure feedback can be as thin as `execution ended as Failed`, without the actual stderr, exit code, or execution evidence.

   修复方向 / Fix direction: 为异常或结果不确定的动作提供明确的恢复/人工处理路径，注册输出 Artifact，并将脱敏后的失败证据交给下一轮。Provide explicit recovery/review handling for exceptional or uncertain outcomes, register output Artifacts, and attach sanitized execution evidence to the next pass.

   Evidence: [Platform output](../src/platform.rs#L166), [execution errors](../src/scheduler.rs#L825), [verification dispatch](../src/runner.rs#L351), [Artifact API](../src/api.rs#L430), [failure feedback](../src/runner.rs#L411).

以下是需要对齐的设计选择，不能直接算成 bug。

The following are design choices to align on, not automatically bugs.

| 中文 | English |
| --- | --- |
| 当前一个 Issue 可以有多轮 Job。若要求严格一对一，需要改 Job 的生命周期定义；去掉 WorkOrder 并不自动解决这个问题。 | An Issue currently has multiple Job passes. A strict one-to-one relationship requires a different Job lifecycle definition; removing WorkOrder does not decide that relationship. |
| Failed 分类同时收纳 Job 和 Action 的失败。需要确定界面是否保留合并分类，或提供子分类。 | The Failed category contains both Job and Action failures. Decide whether to keep the combined view or expose subcategories. |
| 执行层目前是 runbook 命令和目标锁。只有在明确需要额外智能决策时，才需要讨论另一个模型驱动的执行 Agent。 | The execution layer is currently runbook commands plus target locks. An additional model-driven executor is a separate requirement to define if needed. |

验证边界：同一提交在上一轮检查中通过了 57 项 Rust 测试和 Web 构建。Inbox 测试使用模拟模型和 dry-run；本轮另行复现了子进程超时行为。真实模型、真实动作、失败恢复及业务结案的端到端验收尚未在这些检查中完成。定时采集、自动异常建单和持续自主排障也仍属于未接通的能力。

Validation limits: The same commit passed 57 Rust tests and the Web build in the preceding inspection. Inbox tests use a scripted model and dry-run execution; the child-process timeout behavior was reproduced separately in this review. These checks do not complete end-to-end acceptance of a live model, real operations, failure recovery, or business-level Issue resolution. Periodic collection, automatic incident intake, and continuous autonomous troubleshooting also remain unwired capabilities.
