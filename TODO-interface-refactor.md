# TODO - 表现层拆分（CLI / Web UI）

## 目标

- [x] 将表现层职责从 `src/main.rs` 中拆分出去
- [x] 形成清晰的 `interface::cli` 与 `interface::web` 双入口表现层
- [x] 保持现有行为不变（命令参数、UI 路由、输出格式）
- [x] 完成后 `main.rs` 仅负责启动装配与流程编排

## 范围

- [x] 本次先迁移 Web UI 相关代码到 `interface` 层
- [x] CLI 参数结构（`src/interface/cli.rs`）保持不变
- [x] 不改业务规则（review/ci/repo/auth）

## Phase 1 - Web UI 抽离（当前进行中）

- [x] 新增 `src/interface/web.rs`
- [x] 迁移 `UiCommand`、`UiState`
- [x] 迁移 `run_ui_server` 与全部 `ui_*` handler
- [x] 迁移 UI 内部 helper：`authorized`、`ui_redirect`、`html_escape`
- [x] 在 `src/interface/mod.rs` 导出 `web` 模块
- [x] 更新 `src/main.rs`：改为调用 `interface::web`，删除重复定义

## Phase 2 - CLI 执行分发收敛（进行中）

- [x] 将 `main.rs` 的顶层 `match cli.command` 迁移到 `interface` handler 层
- [x] 保留参数定义在 `cli.rs`，新增执行器文件 `src/interface/cli_handlers.rs`
- [x] 保持命令行输出兼容

## Phase 3 - 应用编排函数下沉（进行中）

- [x] 将 `run_review_once`、`run_ci_once`、`run_review_ad_hoc`、`run_review_clean` 迁移到 `application/usecases`
- [x] `main.rs` 仅保留依赖构造 + 入口路由
- [x] 增加最小回归测试，确保迁移前后行为一致

## 验收标准

- [x] `cargo test -q` 全绿
- [x] `cargo run -- --help` 行为一致
- [x] `review daemon --ui` 页面、路由、动作行为一致
- [x] `main.rs` 不再包含 Web handler 与 HTML 拼接代码
