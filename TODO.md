# PrismFlow TODO - 任务链 Context 与优雅关闭

## 1. 背景

当前 `review daemon` / `ci daemon` 已有 `Ctrl+C` 监听，但仍存在以下问题：

- 外部命令（`git clone/fetch/checkout`、引擎命令）使用阻塞调用，不能及时取消。
- 任务取消缺少统一上下文（Context）贯穿整条任务链。
- 关闭流程未实现“先协作取消，再超时强制结束子进程”的完整语义。

## 2. 目标

- 建立统一 `TaskContext`，串联一次 cycle 内的任务链。
- 支持 `Ctrl+C` 后快速、可预期地取消 in-flight 任务。
- 实现可观测的优雅关闭流程（日志、阶段、超时、退出码语义）。
- 保证改造后不破坏现有命令行行为和配置兼容性。

## 3. 非目标

- 不在本次改造中重写业务规则（审查策略、评论格式、标签策略）。
- 不在本次改造中引入分布式任务队列或持久化调度器。
- 不在本次改造中处理 GitHub API 分页重构（单独 backlog）。

## 4. 实施计划

## Phase A - 设计与骨架

- [ ] 新增 `TaskContext`（建议放在 `src/application/context.rs`）：
- [ ] 字段：`run_id`、`CancellationToken`、`started_at`、`deadline`（可选）
- [ ] 字段：子进程注册表（`Arc<Mutex<HashMap<...>>>`）
- [ ] 提供 API：`is_cancelled`、`cancel`、`cancelled().await`
- [ ] 提供 API：`register_child`、`unregister_child`、`kill_all_children`
- [ ] 在 `src/application/mod.rs` 导出模块

- [ ] 为 `review` 与 `ci` 的一次 cycle 创建独立 `TaskContext`
- [ ] `run_review_once` / `run_ci_once` 增加 `ctx` 参数并向下透传
- [ ] 所有长耗时步骤在入口处加 `cancel` 预检查

## Phase B - 外部命令可取消化

- [ ] 将同步 `std::process::Command` 改为 `tokio::process::Command`
- [ ] 改造点：`src/application/review_workflow.rs::prepare_repo_checkout`
- [ ] 改造点：`src/application/ci_workflow.rs::prepare_repo_checkout`
- [ ] 改造点：`src/infrastructure/shell_adapter.rs::run_command_line_in_dir`

- [ ] 为外部命令增加统一执行函数（建议 `run_child_with_cancel`）：
- [ ] 执行前注册 child 到 `TaskContext`
- [ ] `tokio::select!` 同时等待 `child.wait()` 与 `ctx.cancelled()`
- [ ] 取消时发送 kill，等待有限超时后返回 `Cancelled` 错误
- [ ] 无论成功/失败/取消都确保 `unregister_child`

## Phase C - daemon 关停编排

- [ ] `Ctrl+C` 首次触发：
- [ ] 标记 daemon shutdown 状态
- [ ] 调用 `ctx.cancel()`
- [ ] 打印 shutdown 原因与当前 in-flight 数

- [ ] 进入“宽限期”（例如 5-10 秒）等待任务自然结束
- [ ] 宽限期超时后执行 `ctx.kill_all_children()`
- [ ] 等待后台任务收尾（status task、ui task、stdin task）
- [ ] 输出最终 shutdown summary

- [ ] 处理二次 `Ctrl+C`：
- [ ] 直接跳过宽限期，立即强制 kill 并退出

## Phase D - 错误模型与可观测性

- [ ] 扩展错误类型（建议 `DomainError` 或 `application` 层错误）：
- [ ] `CancelledBySignal`
- [ ] `CancelledByOperator`
- [ ] `ChildProcessKilled`

- [ ] 日志结构化字段统一：
- [ ] `run_id`、`repo`、`pr_number`、`stage`、`elapsed_ms`
- [ ] `cancel_reason`、`child_pid`、`command_fingerprint`

- [ ] 在 status 流中增加：
- [ ] `cycle:cancel_requested`
- [ ] `cycle:graceful_shutdown_begin/end`
- [ ] `cycle:force_kill_begin/end`

## Phase E - 测试与验收

- [ ] 单元测试：
- [ ] `TaskContext` 注册/反注册/取消/批量 kill 行为
- [ ] 命令执行包装器在取消时返回确定性错误

- [ ] 集成测试（可用 mock child 或本地长命令）：
- [ ] review 运行中触发取消，验证不再继续新 PR
- [ ] clone 阶段取消，验证子进程被结束
- [ ] 引擎命令阶段取消，验证不再回写评论

- [ ] 回归测试：
- [ ] 保持现有 14 个测试通过
- [ ] 新增取消相关测试后，`cargo test -q` 全绿

## 5. 验收标准（Definition of Done）

- [ ] 在 `git clone`、`git fetch`、`git checkout`、引擎命令执行中按 `Ctrl+C`，可在可接受时间内退出（目标 < 3 秒，最差 < 10 秒）。
- [ ] 取消后不再调度新的 repo/pr 任务。
- [ ] 取消后不会留下孤儿子进程。
- [ ] 关闭日志可明确区分：自然结束 / 协作取消 / 强制 kill。
- [ ] `review daemon` 和 `ci daemon` 两条链路行为一致。

## 6. 代码落点清单

- [ ] `src/main.rs`
- [ ] `src/application/review_workflow.rs`
- [ ] `src/application/ci_workflow.rs`
- [ ] `src/infrastructure/shell_adapter.rs`
- [ ] `src/application/mod.rs`
- [ ] `src/domain/errors.rs`（如需补充取消类错误）
- [ ] `README.md`（补充取消语义与 graceful shutdown 说明）

## 7. 风险与注意事项

- [ ] Windows 下子进程树 kill 语义与 Unix 不同，需显式验证。
- [ ] 避免在持锁时执行 `.await`，防止死锁。
- [ ] 取消与重试逻辑耦合时，取消应高优先级于重试。
- [ ] 处理中断时要保证临时文件清理（`tmp-diffs`、`tmp-ci`）。

## 8. 后续可选增强（Backlog）

- [ ] GitHub API 分页完整化（避免漏 PR/评论/文件）。
- [ ] `main.rs` / `review_workflow.rs` 进一步拆分，降低维护成本。
- [ ] UI token 从 query 参数迁移到 header/cookie，降低泄露风险。

