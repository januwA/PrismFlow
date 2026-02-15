# PrismFlow

PrismFlow 是一个面向 GitHub Pull Request 的自动化分析工具，支持：

- 仓库管理（监控列表、Agent 绑定）
- PR 扫描（只读）
- 代码审查（调用本地引擎并回写评论）
- CI 失败分析（读取失败检查并给出修复建议）

---

## 环境要求

- Rust（stable）
- Git
- GitHub Token（建议具备 `repo` 相关权限）

---

## 快速开始

```bash
cargo build
cargo run -- --help
```

### 鉴权

```bash
cargo run -- auth login <GITHUB_TOKEN>
cargo run -- auth which
```

---

## 常用命令

### 仓库管理

```bash
cargo run -- repo add owner/repo
cargo run -- repo list
cargo run -- repo remove owner/repo
```

### 扫描（不依赖引擎）

```bash
cargo run -- scan once
cargo run -- scan once --repo owner/a --exclude-repo owner/b
```

### 代码审查（必须传 `--engine`）

```bash
cargo run -- review once --engine qwen-v1 "qwen -y \"diff={patch_file}\""
cargo run -- review daemon --engine qwen-v1 "qwen -y \"diff={patch_file}\""
cargo run -- review ad-hoc https://github.com/owner/repo/pull/123 --engine qwen-v1 "qwen -y \"diff={patch_file}\""
```

### CI 失败分析（必须传 `--engine`）

```bash
cargo run -- ci once --engine gemini-cli "gemini -y -p \"$(cat {ci_file})\""
cargo run -- ci daemon --interval-secs 60 --engine gemini-cli "gemini -y -p \"$(cat {ci_file})\""
```

---

## 关键参数与占位符

- `--shell "<path>"`：指定命令执行 shell（例如 `D:\apps\Git\bin\bash.exe`）
- `--repo` / `--exclude-repo`：包含/排除仓库（可重复）
- `--clone-repo`：启用仓库 clone 上下文
- `--clone-workspace-dir`：可指定 clone 缓存目录（`review` 与 `ci` 建议使用不同目录）

可用占位符：

- 审查：`{patch_file}`、`{agents_file}`、`{changed_files_file}`
- CI：`{ci_file}`
- clone 上下文：`{repo_dir}`、`{repo_head_sha}`、`{repo_head_ref}`

并行运行建议（推荐）：

- 终端 A：`review daemon`
- 终端 B：`ci daemon`
- 使用默认目录时已分离：`review => .prismflow/repo-cache`，`ci => .prismflow/ci-repo-cache`

---

## 测试与格式化

```bash
cargo fmt
cargo test -q
```

---

## 配置位置

- 仓库配置：`pr-reviewer/repos.json`
- Token 文件：`pr-reviewer/auth_token`
- Windows 示例：`C:\Users\<用户名>\AppData\Roaming\pr-reviewer\...`
