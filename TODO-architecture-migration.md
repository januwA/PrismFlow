# PrismFlow 架构依赖迁移 TODO

## 目标
- 消除 `application -> infrastructure` 依赖。
- 将 Shell 抽象上移为内层 Port，保持“依赖指向内部”。
- 保持现有功能不回退（review/ci/daemon/cancel）。

## 范围
- `src/domain/ports.rs`
- `src/infrastructure/shell_adapter.rs`
- `src/application/usecases.rs`
- `src/application/review_workflow.rs`
- `src/application/ci_workflow.rs`
- `src/interface/cli_handlers.rs`
- `src/main.rs`

## Phase 0: 基线确认
- [x] 记录当前 `application` 中所有 `use crate::infrastructure::...` 引用。
- [x] 记录 `ShellAdapter` 当前定义与实现位置。
- [x] 运行一次基线测试（至少 `cargo test -q`）。

## Phase 1: 上移 Shell Port
- [x] 在 `src/domain/ports.rs` 新增 `ShellAdapter` trait 定义（含 async 方法签名）。
- [x] 从 `src/infrastructure/shell_adapter.rs` 删除 trait 定义，仅保留 `CommandShellAdapter` 实现。
- [x] 让 `CommandShellAdapter` 改为 `impl crate::domain::ports::ShellAdapter`。
- [x] 修复编译错误并跑测试。

## Phase 2: 清理 Application 层依赖方向
- [x] `src/application/review_workflow.rs` 改为引用 `domain::ports::ShellAdapter`。
- [x] `src/application/ci_workflow.rs` 改为引用 `domain::ports::ShellAdapter`。
- [x] 检查 `application` 子模块中是否还存在 `use crate::infrastructure::...`。
- [x] 若仍存在，继续替换为 ports trait 或上移抽象。

## Phase 3: 收敛 Usecase 参数（去具体实现）
- [x] `run_review_once` 等 usecase 参数从 `&CommandShellAdapter` 改为 `&dyn ShellAdapter`。
- [x] `config_repo` 参数从 `&LocalConfigAdapter` 改为 `&dyn ConfigRepository`（如可行）。
- [x] `github_client_for_action` 与调用关系梳理，避免 application 暴露基础设施具体类型。
- [x] 确保用例层仅依赖 `domain::ports` 与 `application` 内部类型。

## Phase 4: 组合根注入
- [x] 在 `main` / `interface` 层构造 `CommandShellAdapter`、`LocalConfigAdapter`、`OctocrabGitHubRepository`。
- [x] 通过 trait 引用注入到 usecase/workflow。
- [x] 确认 `application` 不再直接 import `infrastructure`。

## Phase 5: TaskContext 边界优化（可选但建议）
- [x] 评估是否将 `TaskContext` 收敛为更窄接口（如 `CancellationContext`）。
- [x] 如实施：在内层定义取消相关最小 trait，infra 只依赖该 trait。
- [x] 保留 `run_id/deadline/child_process` 能力但避免泄露过多应用语义。
- [x] 在 `domain::ports` 新增 `ProcessManager`，将 OS kill 实现下沉到 `infrastructure/process_manager.rs`。
- [x] `TaskContext` 移除直接 OS 命令调用，改为通过 `ProcessManager` 执行进程终止。

## 验收标准
- [x] `rg -n "use crate::infrastructure::" src/application` 无结果。
- [x] `ShellAdapter` trait 不在 `src/infrastructure/*` 定义。
- [ ] 功能回归通过：review once / ci once / daemon cancel（Ctrl+C）路径。
- [x] `cargo test -q` 全量通过。

## 风险与回滚
- [ ] 风险：async trait 签名迁移导致大面积编译错误。
- [ ] 风险：对象安全（`dyn trait`）与生命周期问题。
- [ ] 回滚策略：按 Phase 分提交，任一阶段失败可 `git revert <commit>` 回退。

## 提交计划（建议）
- [ ] Commit 1: move shell port to domain.
- [ ] Commit 2: application imports cleanup.
- [ ] Commit 3: usecase signature decoupling.
- [ ] Commit 4: composition root wiring + tests.
