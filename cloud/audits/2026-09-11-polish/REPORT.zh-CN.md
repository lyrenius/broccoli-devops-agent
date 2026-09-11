# Agent 打磨与原 VPS 部署验收

日期：2026-09-11。执行环境：原 5 号 VPS `36.103.236.92`；入口 `http://127.0.0.1:15184/`，SSH 转发到 Nginx 5184，API 14724 转发到 4724。

## 逐项修复

| 项目 | 提交 | 最终行为与证据 |
|---|---|---|
| B-01 观测缺口 | `f2ef1cb` | 缺凭据、认证失败、上游 API 失败和畸形数据作为 coverage gap；成功读取但缺少目标心跳仍属异常。`tests/probe_coverage.rs`、`tests/reliability.rs` 覆盖对照与部分覆盖。 |
| B-02 生命周期 | `3c93606` | Issue 关闭、future drop、终态准入、重启清理均有保护。5 项 `job_lifecycle` 测试覆盖取消隔离、未执行动作、旧状态恢复；本轮真实 HTTP 断开后 Job 自动取消。 |
| B-06 验证探针 | `1337d4a` | verification probe 逐目标核对；缺失时拒绝提议，多目标和省略自定义探针均有测试。 |
| 既有 runbook 入库 | `eb9188e` | 诊断与恢复矩阵、资源作用域和机器适配器版本化。原 19 个配置 runbook 保留；实际 worker.start 恢复通过。 |
| B-04 Trace 断流 | `1515b0d` | 连接回调驱动状态，失败时轮询，重连时重新加载，重放去重。单测和真实 SSH 断开/恢复均通过。 |
| B-03 等待状态 | `cc12ca9` | 展示运行时长、最后进展、模型等待/重试阶段，移除固定一分钟承诺。真实 Luna 运行时观察到 started_at、last_progress_at、模型轮次和工具调用进度。 |
| B-05 上报隔离 | `88062cb` | 客户端 report_id 关联服务端早期准入，再绑定 Issue/Job；并发上报及背景事件不混入另一窗口。Rust 并发测试、前端交错重放测试和本地双窗口验收通过。 |
| B-07 真实归档 UI 导入 | `9469c63` | 原 UI 先 parse/stringify，改变 0.0 或大整数，导致未修改的合法文件哈希失败。现在直接发送文件原文，服务器继续执行原有完整性校验；数值精度回归测试和同一 VPS 文件 UI 导入通过。 |

原 27 分钟等待没有完整现场证据，不能把它统一归因于某一个原因。本轮修复和验证了已确认的生命周期及状态展示问题，不宣称重建了那次故障的全部因果链。

## 验证

- Rust：`cargo test --workspace`，162 项通过；`cargo clippy --workspace --all-targets -- -D warnings` 通过。
- 前端：`cd web && npm test && npm run build`，6 项测试通过，TypeScript 和生产构建通过。
- Python operational helper：4 项通过。
- `git diff --check` 通过。
- 保持既有 R-01～R-05 行为：未实现 runbook 交给人工；关闭说明/多轮记录/搜索/Trace 跳转保留；Mtoks 两位小数。本轮未重新设计这些已实现入口。

### 实机恢复

先确认七个资源健康、队列为零，再为 worker-1 设 180 秒独立恢复定时器，停止 worker-1 并提交明确标记的验收报告。Luna 第一轮完成诊断并执行一个成功恢复动作，第二轮验证解决。after-Snapshot 七个资源全部 Healthy、coverage_gaps 为空；随后撤销备用定时器并关闭本轮测试 Issue。

恢复后的实际判题：C++ 正确答案 Accepted（submission 31）、Python 正确答案 Accepted（32）、C++ 错误答案 WrongAnswer（33）。详见 `worker-recovery.json`、`snapshot-after-recovery.json`、`judging-after.json`。

### 断线、取消与重启

- 在只读 Luna 任务运行时主动关闭本任务的 SSH 控制连接。
- Trace 实际显示 `not connected` 和 API unreachable。
- 服务器于 `2026-09-11T15:23:44.284904779Z` 记录 `scheduler.job_cancelled`：request/future 被丢弃；随即将 Issue 调整为 WaitingForHuman。
- SSH 恢复后页面无需刷新即更新 Job 终态；随后于 `15:24:46Z` 关闭测试 Issue，运行列表为空，无动作产生。`cancel-live.json` 中的约 0.97 秒是关闭接口及确认清空的耗时，不能当作网络断开到取消的延迟。
- 重启新版本后为 Running、无运行 Job；配置与 model/probe 环境文件均与升级前备份字节一致。

### 归档与设置

- 本地简化样例验证导出、导入、归档不进 Inbox、拒绝反馈重启任务、设置跨进程重启持久化。
- 设置页实际修改自动轮次 3→2，点击 Save，刷新后仍为 2。
- 真实 VPS 恢复会话导出后，用文件选择器导入隔离的本地实例。修复前复现哈希失败；修复后同一文件成功，保留 2 个 Job、1 个动作、7 个 artifact、55 个事件，显示 archive。
- 线上实例未导入验收归档；保持原 Luna 数据目录与共享五机拓扑，不串用 Sol 数据。
- 最新实机快照确认 DLQ unresolved=0，无需再删除。

## 版本与回退

后端从干净 Git archive 的 `88062cbaeee3c8f74f38c9566bd001e1fe155d9b` 在 VPS 构建 release，3 分 19 秒完成。实际 `/proc/<pid>/exe` 与发布文件 SHA-256 相同：

`c560d84fcb3b2d1a82bdace1ab35291d1f079ca39cf2c613d8c41c2740aff1e7`

后续 `9469c6370ffbeee991af6c31051d1c81c3931b11` 仅修复前端导入，复用上述未变更的后端。最新前端与后端分别标注于 `/deployment-version.json`，哈希与本地生产构建比对。

- 后端：`/opt/broccoli-agent-validation/88062cb/broccoli-devops-agent`。
- 前端：`/opt/broccoli-agent-validation/9469c63/console`。
- 服务：`broccoli-agent-luna-1968090.service`，`99-polish.conf` 明确覆盖启动路径；服务名中的旧提交不是版本证据。
- 配置：`/var/lib/broccoli-agent/validation/luna-1968090/agent.toml`；Luna、英文、dry_run=false、snapshot_interval_secs=0 均未改变。
- 升级前备份：`/var/lib/broccoli-agent/validation/backups/pre-88062cb-20260911`，root 私有，含数据、配置、环境及服务文件。
- 回退新增前端：将 Nginx 的 root 切回 `/opt/broccoli-agent-validation/88062cb/console`，先 `nginx -t` 再 reload。
- 回退本轮后端：停止 Luna 服务，移除本轮 `99-polish.conf` 覆盖，恢复备份中的 Nginx 配置，daemon-reload 后启动。旧二进制与旧覆盖文件仍保留。不要直接覆盖当前数据；需要恢复旧数据时先另存升级后的数据，避免丢失新记录。

本次 SSH 转发已恢复，但仍依赖本机 SSH 会话存活。
