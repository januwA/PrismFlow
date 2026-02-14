use clap::{ArgAction, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "prismflow",
    version,
    about = "异步 PR 审查编排器",
    long_about = "PrismFlow：用于扫描并自动审查 GitHub Pull Request 的命令行工具。",
    after_long_help = "顶层 COMMAND 可选：\n  repo    仓库管理\n  auth    认证管理\n  scan    扫描模式（只读）\n  review  审查模式（会写评论）\n\n示例：\n  cargo run -- repo list\n  cargo run -- scan once\n  cargo run -- review daemon --interval-secs 30"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    #[command(about = "仓库管理（添加/删除/查看监控仓库）")]
    Repo(RepoCommand),
    #[command(about = "认证管理（保存 Token、查看当前 Token 来源）")]
    Auth(AuthCommand),
    #[command(about = "扫描模式（只读取 PR，不提交评论）")]
    Scan(ScanCommand),
    #[command(about = "审查模式（会提交评论，可单次或守护运行）")]
    Review(ReviewCommand),
}

#[derive(Debug, Args)]
#[command(after_long_help = "repo COMMAND 可选：\n  add <owner/repo|github_url>         添加监控仓库（支持仓库/PR URL）\n  remove [owner/repo]                 移除监控仓库（不带参数时可交互选择）\n  list                                列出监控仓库\n  agent add <owner/repo> <agent>      给仓库绑定 Agent\n  agent remove <owner/repo> <agent>   解绑 Agent\n  agent list [owner/repo]             列出单仓库或全部仓库 Agent\n  agent dir-add <dir>                 添加全局 Agent 提示词搜索目录（所有仓库共享）\n  agent dir-remove <dir>              移除全局 Agent 提示词搜索目录\n  agent dir-list                      列出全局目录\n\n示例：\n  cargo run -- repo add owner/repo\n  cargo run -- repo add https://github.com/owner/repo/pull/123\n  cargo run -- repo agent add owner/repo security\n  cargo run -- repo agent dir-add .prismflow/prompts\n  cargo run -- repo agent dir-list\n  cargo run -- repo remove\n  cargo run -- repo remove owner/repo")]
pub struct RepoCommand {
    #[command(subcommand)]
    pub command: RepoSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoSubcommand {
    #[command(about = "添加监控仓库，支持 owner/repo 或 GitHub URL")]
    Add { full_name: String },
    #[command(about = "移除监控仓库，支持 owner/repo；不传参数时交互选择")]
    Remove { full_name: Option<String> },
    #[command(about = "列出当前监控仓库")]
    List,
    #[command(about = "管理仓库绑定的 Agent 列表")]
    Agent(RepoAgentCommand),
}

#[derive(Debug, Args)]
pub struct RepoAgentCommand {
    #[command(subcommand)]
    pub command: RepoAgentSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoAgentSubcommand {
    #[command(about = "给仓库添加 Agent")]
    Add { full_name: String, agent: String },
    #[command(about = "从仓库移除 Agent")]
    Remove { full_name: String, agent: String },
    #[command(about = "列出仓库已绑定的 Agent")]
    List { full_name: Option<String> },
    #[command(about = "添加全局 Agent 提示词搜索目录（所有仓库共享）")]
    DirAdd { dir: String },
    #[command(about = "移除全局 Agent 提示词搜索目录")]
    DirRemove { dir: String },
    #[command(about = "列出全局 Agent 提示词搜索目录")]
    DirList,
}

#[derive(Debug, Args)]
#[command(after_long_help = "auth COMMAND 可选：\n  login <token>  保存 Token\n  which          查看当前生效 Token 来源\n\n示例：\n  cargo run -- auth login ghp_xxx\n  cargo run -- auth which")]
pub struct AuthCommand {
    #[command(subcommand)]
    pub command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum AuthSubcommand {
    #[command(about = "登录并保存 GitHub Token 到本地配置")]
    Login { token: String },
    #[command(about = "查看当前生效的 Token 来源（gh/env/本地存储）")]
    Which,
}

#[derive(Debug, Args)]
#[command(after_long_help = "scan COMMAND 可选：\n  once [--engine-fingerprint <值>] [--max-concurrent-api <值>]\n\n说明：\n  scan 只读取 PR 信息和幂等键，不会提交评论。\n\n示例：\n  cargo run -- scan once\n  cargo run -- scan once --engine-fingerprint qwen-v1 --max-concurrent-api 12")]
pub struct ScanCommand {
    #[command(subcommand)]
    pub command: ScanSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ScanSubcommand {
    #[command(about = "执行一次扫描（只输出待审查信息与锚点键，不写评论）")]
    Once {
        #[arg(long, default_value = "default-engine", help = "审查引擎指纹，用于幂等去重键计算")]
        engine_fingerprint: String,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
    },
}

#[derive(Debug, Args)]
#[command(after_long_help = "review COMMAND 可选：\n  once [--engine-fingerprint <值>] [--engine-command <命令>] [--engine <指纹> <命令>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--agent <名称>] [--keep-diff-files] [--max-concurrent-repos <值>] [--max-concurrent-prs <值>] [--max-concurrent-api <值>]\n  daemon [--interval-secs <秒>] [--ui] [--ui-bind <地址>] [--ui-token <口令>] [--engine-fingerprint <值>] [--engine-command <命令>] [--engine <指纹> <命令>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--agent <名称>] [--keep-diff-files] [--max-concurrent-repos <值>] [--max-concurrent-prs <值>] [--max-concurrent-api <值>]\n  ad-hoc <pr_url> [--engine-fingerprint <值>] [--engine-command <命令>] [--engine <指纹> <命令>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--agent <名称>] [--keep-diff-files] [--max-concurrent-api <值>]\n\n说明：\n  review 会向 GitHub 提交评论。\n  review 固定使用 shell 模式，本地命令支持 {patch_file} 占位符。\n  --engine-prompt 或 --engine-prompt-file 会注入到 patch_file 顶部，便于定制模型行为。\n  --engine-prompt 和 --engine-prompt-file 不能同时使用。\n  --engine 可重复传入，每次必须给两个参数：<fingerprint> <command>，按 PR 轮询使用。\n  --engine 与 --engine-command 不能同时使用。\n  --agent 会先按全局 agent 目录配置查找；再回退到 当前目录/.prismflow/prompts 与系统配置目录 pr-reviewer/prompts。\n  --ui 启动本地 HTTP 管理页面；若开放局域网访问，建议同时设置 --ui-token。\n  默认会自动删除生成的 diff 文件；加 --keep-diff-files 可保留到 .prismflow/tmp-diffs。\n\n示例：\n  cargo run -- review once --engine qwen-v1 \"qwen -y \\\"diff: {patch_file}\\\"\" --engine ollama:qwen2.5:1.5b \"cat {patch_file} | ollama run qwen2.5:1.5b\" --engine-prompt-file prompts/bug-only.txt --agent logic --keep-diff-files --max-concurrent-repos 3 --max-concurrent-prs 6 --max-concurrent-api 12\n  cargo run -- review ad-hoc https://github.com/owner/repo/pull/123 --engine ollama:qwen2.5:1.5b \"cat {patch_file} | ollama run qwen2.5:1.5b\" --engine-prompt-file prompts/bug-only.txt --agent security\n  cargo run -- review daemon --ui --ui-bind 0.0.0.0:8787 --ui-token mysecret --interval-secs 30 --engine qwen-v1 \"qwen -y \\\"diff: {patch_file}\\\"\" --max-concurrent-repos 3 --max-concurrent-prs 6 --max-concurrent-api 12")]
pub struct ReviewCommand {
    #[command(subcommand)]
    pub command: ReviewSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ReviewSubcommand {
    #[command(about = "执行一次审查（会尝试提交 inline 评论或降级总结评论）")]
    Once {
        #[arg(long, default_value = "default-engine", help = "审查引擎指纹，用于幂等去重键计算")]
        engine_fingerprint: String,
        #[arg(long, help = "shell 模式命令，例如：qwen --review --input {patch_file}（支持 {patch_file} 占位符）")]
        engine_command: Option<String>,
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用（与 --engine-command 互斥）"
        )]
        engines: Vec<String>,
        #[arg(long, help = "追加到 patch_file 顶部的自定义提示词（建议 shell 模式使用）")]
        engine_prompt: Option<String>,
        #[arg(long, help = "提示词文件路径，读取文件内容并追加到 patch_file 顶部（建议用于多行提示词）")]
        engine_prompt_file: Option<String>,
        #[arg(long = "agent", help = "启用的 Agent 名称（可重复，如 --agent security --agent logic）")]
        agents: Vec<String>,
        #[arg(long, default_value_t = false, help = "保留生成的 diff 文件（默认审查后自动删除）")]
        keep_diff_files: bool,
        #[arg(long, default_value_t = 2, help = "仓库并发数")]
        max_concurrent_repos: usize,
        #[arg(long, default_value_t = 4, help = "单仓库 PR 并发数")]
        max_concurrent_prs: usize,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
    },
    #[command(about = "守护模式持续审查（按间隔轮询执行）")]
    Daemon {
        #[arg(long, default_value_t = 60, help = "轮询间隔（秒）")]
        interval_secs: u64,
        #[arg(long, default_value_t = false, help = "启用本地 HTTP 管理页面")]
        ui: bool,
        #[arg(long, default_value = "127.0.0.1:8787", help = "UI 监听地址，例如 127.0.0.1:8787 或 0.0.0.0:8787")]
        ui_bind: String,
        #[arg(long, help = "UI 访问口令（建议在局域网访问时设置）")]
        ui_token: Option<String>,
        #[arg(long, default_value = "default-engine", help = "审查引擎指纹，用于幂等去重键计算")]
        engine_fingerprint: String,
        #[arg(long, help = "shell 模式命令，例如：qwen --review --input {patch_file}（支持 {patch_file} 占位符）")]
        engine_command: Option<String>,
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用（与 --engine-command 互斥）"
        )]
        engines: Vec<String>,
        #[arg(long, help = "追加到 patch_file 顶部的自定义提示词（建议 shell 模式使用）")]
        engine_prompt: Option<String>,
        #[arg(long, help = "提示词文件路径，读取文件内容并追加到 patch_file 顶部（建议用于多行提示词）")]
        engine_prompt_file: Option<String>,
        #[arg(long = "agent", help = "启用的 Agent 名称（可重复，如 --agent security --agent logic）")]
        agents: Vec<String>,
        #[arg(long, default_value_t = false, help = "保留生成的 diff 文件（默认审查后自动删除）")]
        keep_diff_files: bool,
        #[arg(long, default_value_t = 2, help = "仓库并发数")]
        max_concurrent_repos: usize,
        #[arg(long, default_value_t = 4, help = "单仓库 PR 并发数")]
        max_concurrent_prs: usize,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
    },
    #[command(about = "即时审查单个 PR URL（无需预先加入 repo 列表）")]
    AdHoc {
        #[arg(help = "GitHub PR 链接，例如 https://github.com/owner/repo/pull/123")]
        pr_url: String,
        #[arg(long, default_value = "default-engine", help = "审查引擎指纹，用于幂等去重键计算")]
        engine_fingerprint: String,
        #[arg(long, help = "shell 模式命令，例如：qwen --review --input {patch_file}（支持 {patch_file} 占位符）")]
        engine_command: Option<String>,
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用（与 --engine-command 互斥）"
        )]
        engines: Vec<String>,
        #[arg(long, help = "追加到 patch_file 顶部的自定义提示词（建议 shell 模式使用）")]
        engine_prompt: Option<String>,
        #[arg(long, help = "提示词文件路径，读取文件内容并追加到 patch_file 顶部（建议用于多行提示词）")]
        engine_prompt_file: Option<String>,
        #[arg(long = "agent", help = "启用的 Agent 名称（可重复，如 --agent security --agent logic）")]
        agents: Vec<String>,
        #[arg(long, default_value_t = false, help = "保留生成的 diff 文件（默认审查后自动删除）")]
        keep_diff_files: bool,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
    },
    #[command(about = "清理指定 PR 上 PrismFlow 留下的评论与标签痕迹（尽力而为）")]
    Clean {
        #[arg(help = "GitHub PR 链接，例如 https://github.com/owner/repo/pull/123")]
        pr_url: String,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
    },
}
