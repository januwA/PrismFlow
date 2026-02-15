use clap::{ArgAction, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "prismflow",
    version,
    about = "异步 PR 审查编排器",
    long_about = "PrismFlow：用于扫描并自动审查 GitHub Pull Request 的命令行工具。",
    after_long_help = "顶层 COMMAND 可选：\n  repo    仓库管理\n  auth    认证管理\n  scan    扫描模式（只读）\n  ci      CI 失败分析并回写建议\n  review  审查模式（会写评论）\n\n配置文件位置：\n  repos.json 位于系统配置目录下的 pr-reviewer/repos.json\n  Windows 示例：C:\\Users\\<用户名>\\AppData\\Roaming\\pr-reviewer\\repos.json\n  token 文件：同目录下 auth_token\n\n示例：\n  cargo run -- repo list\n  cargo run -- scan once\n  cargo run -- ci once --engine gemini-cli \"gemini -y -p \\\"$(cat {ci_file})\\\"\"\n  cargo run -- review daemon --interval-secs 30"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        help = "指定命令执行 shell（示例：D:\\apps\\Git\\bin\\bash.exe）；默认自动检测当前环境 shell"
    )]
    pub shell: Option<String>,
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
    #[command(about = "CI 分析模式（读取失败 CI 并输出修复建议评论）")]
    Ci(CiCommand),
    #[command(about = "审查模式（会提交评论，可单次或守护运行）")]
    Review(ReviewCommand),
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "repo COMMAND 可选：\n  add <owner/repo|github_url>         添加监控仓库（支持仓库/PR URL）\n  remove [owner/repo]                 移除监控仓库（不带参数时可交互选择）\n  list                                列出监控仓库\n  agent add [owner/repo] <agent>      给仓库绑定 Agent（不带仓库参数时可交互选择）\n  agent remove [owner/repo] <agent>   解绑 Agent（不带仓库参数时可交互选择）\n  agent list [owner/repo]             列出单仓库或全部仓库 Agent\n  agent dir-add <dir>                 添加全局 Agent 提示词搜索目录（所有仓库共享）\n  agent dir-remove <dir>              移除全局 Agent 提示词搜索目录\n  agent dir-list                      列出全局目录\n\nrepos.json 位置：系统配置目录/pr-reviewer/repos.json\nWindows 示例：C:\\Users\\<用户名>\\AppData\\Roaming\\pr-reviewer\\repos.json\n\n示例：\n  cargo run -- repo add owner/repo\n  cargo run -- repo add https://github.com/owner/repo/pull/123\n  cargo run -- repo agent add owner/repo security\n  cargo run -- repo agent add security\n  cargo run -- repo agent dir-add .prismflow/prompts\n  cargo run -- repo agent dir-list\n  cargo run -- repo remove\n  cargo run -- repo remove owner/repo"
)]
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
    Add {
        #[arg(num_args = 1..=2, value_names = ["OWNER_REPO", "AGENT"])]
        args: Vec<String>,
    },
    #[command(about = "从仓库移除 Agent")]
    Remove {
        #[arg(num_args = 1..=2, value_names = ["OWNER_REPO", "AGENT"])]
        args: Vec<String>,
    },
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
#[command(
    after_long_help = "auth COMMAND 可选：\n  login <token>  保存 Token\n  which          查看当前生效 Token 来源\n\nauth_token 位置：系统配置目录/pr-reviewer/auth_token\nWindows 示例：C:\\Users\\<用户名>\\AppData\\Roaming\\pr-reviewer\\auth_token\n\n示例：\n  cargo run -- auth login ghp_xxx\n  cargo run -- auth which"
)]
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
#[command(
    after_long_help = "scan COMMAND 可选：\n  once [--max-concurrent-api <值>] [--repo <owner/repo>] [--exclude-repo <owner/repo>]\n\n说明：\n  scan 只读取 PR 信息和幂等键，不会提交评论，也不依赖引擎参数。\n\n示例：\n  cargo run -- scan once\n  cargo run -- scan once --repo owner/repo --max-concurrent-api 12"
)]
pub struct ScanCommand {
    #[command(subcommand)]
    pub command: ScanSubcommand,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "ci COMMAND 可选：\n  once [--engine <指纹> <命令>] [--clone-repo] [--clone-workspace-dir <目录>] [--clone-depth <值>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--max-concurrent-api <值>] [--repo <owner/repo>] [--exclude-repo <owner/repo>]\n  daemon [--interval-secs <秒>] [--engine <指纹> <命令>] [--clone-repo] [--clone-workspace-dir <目录>] [--clone-depth <值>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--max-concurrent-api <值>] [--repo <owner/repo>] [--exclude-repo <owner/repo>]\n\n说明：\n  ci 会读取 PR 的失败 CI 状态并调用引擎生成修复建议，随后写入 PR 评论。\n  命令支持 {ci_file}、{repo_dir}、{repo_head_sha}、{repo_head_ref} 占位符。\n\n示例：\n  cargo run -- ci once --engine gemini-cli \"gemini -y -p \\\"$(cat {ci_file})\\\"\"\n  cargo run -- ci daemon --interval-secs 60 --engine gemini-cli \"gemini -y -p \\\"$(cat {ci_file})\\\"\""
)]
pub struct CiCommand {
    #[command(subcommand)]
    pub command: CiSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CiSubcommand {
    #[command(about = "执行一次 CI 失败分析并写评论")]
    Once {
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            required = true,
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用"
        )]
        engines: Vec<String>,
        #[arg(long, help = "追加到 ci payload 顶部的自定义提示词")]
        engine_prompt: Option<String>,
        #[arg(long, help = "提示词文件路径，读取内容并追加到 ci payload 顶部")]
        engine_prompt_file: Option<String>,
        #[arg(long, default_value_t = false, help = "启用仓库 clone 上下文模式")]
        clone_repo: bool,
        #[arg(
            long,
            default_value = ".prismflow/ci-repo-cache",
            help = "clone 缓存目录"
        )]
        clone_workspace_dir: String,
        #[arg(long, default_value_t = 1, help = "clone/fetch 深度（最小为 1）")]
        clone_depth: usize,
        #[arg(long, default_value_t = 2, help = "仓库并发数")]
        max_concurrent_repos: usize,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
        #[arg(
            long = "repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "仅处理指定仓库（owner/repo 或 github URL，可重复）"
        )]
        repos: Vec<String>,
        #[arg(
            long = "exclude-repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "排除指定仓库（owner/repo 或 github URL，可重复）"
        )]
        exclude_repos: Vec<String>,
    },
    #[command(about = "守护模式持续执行 CI 失败分析")]
    Daemon {
        #[arg(long, default_value_t = 60, help = "轮询间隔（秒）")]
        interval_secs: u64,
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            required = true,
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用"
        )]
        engines: Vec<String>,
        #[arg(long, help = "追加到 ci payload 顶部的自定义提示词")]
        engine_prompt: Option<String>,
        #[arg(long, help = "提示词文件路径，读取内容并追加到 ci payload 顶部")]
        engine_prompt_file: Option<String>,
        #[arg(long, default_value_t = false, help = "启用仓库 clone 上下文模式")]
        clone_repo: bool,
        #[arg(
            long,
            default_value = ".prismflow/ci-repo-cache",
            help = "clone 缓存目录"
        )]
        clone_workspace_dir: String,
        #[arg(long, default_value_t = 1, help = "clone/fetch 深度（最小为 1）")]
        clone_depth: usize,
        #[arg(long, default_value_t = 2, help = "仓库并发数")]
        max_concurrent_repos: usize,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
        #[arg(
            long = "repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "仅处理指定仓库（owner/repo 或 github URL，可重复）"
        )]
        repos: Vec<String>,
        #[arg(
            long = "exclude-repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "排除指定仓库（owner/repo 或 github URL，可重复）"
        )]
        exclude_repos: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScanSubcommand {
    #[command(about = "执行一次扫描（只输出待审查信息与锚点键，不写评论）")]
    Once {
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
        #[arg(
            long = "repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "仅处理指定仓库（owner/repo 或 github URL，可重复）"
        )]
        repos: Vec<String>,
        #[arg(
            long = "exclude-repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "排除指定仓库（owner/repo 或 github URL，可重复）"
        )]
        exclude_repos: Vec<String>,
    },
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "review COMMAND 可选：\n  once [--engine <指纹> <命令>] [--clone-repo] [--clone-workspace-dir <目录>] [--clone-depth <值>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--agent <名称>] [--keep-diff-files] [--max-concurrent-repos <值>] [--max-concurrent-prs <值>] [--max-concurrent-api <值>]\n  daemon [--interval-secs <秒>] [--ui] [--ui-bind <地址>] [--ui-token <口令>] [--engine <指纹> <命令>] [--clone-repo] [--clone-workspace-dir <目录>] [--clone-depth <值>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--agent <名称>] [--keep-diff-files] [--max-concurrent-repos <值>] [--max-concurrent-prs <值>] [--max-concurrent-api <值>]\n  ad-hoc <pr_url> [--engine <指纹> <命令>] [--clone-repo] [--clone-workspace-dir <目录>] [--clone-depth <值>] [--engine-prompt <提示词>] [--engine-prompt-file <文件>] [--agent <名称>] [--keep-diff-files] [--max-concurrent-api <值>]\n\n说明：\n  review 会向 GitHub 提交评论。\n  review 固定使用 shell 模式，本地命令支持 {patch_file}、{agents_file} 与 {changed_files_file} 占位符。\n  启用 --clone-repo 后，可额外使用 {repo_dir}、{repo_head_sha}、{repo_head_ref} 占位符。\n  --clone-depth 控制 clone/fetch 深度，默认 1；设置为 2 可获取最近两个提交用于对比。\n  {patch_file} 仅包含 diff 内容；{agents_file} 包含 --engine-prompt/--engine-prompt-file 与 Agent 汇总内容；{changed_files_file} 仅包含变更文件名列表。\n  --engine-prompt 和 --engine-prompt-file 不能同时使用。\n  --engine 可重复传入，每次必须给两个参数：<fingerprint> <command>，按 PR 轮询使用。\n  --agent 会先按全局 agent 目录配置查找；再回退到 当前目录/.prismflow/prompts 与系统配置目录 pr-reviewer/prompts。\n  --ui 启动本地 HTTP 管理页面；若开放局域网访问，建议同时设置 --ui-token。\n  默认会自动删除生成的 diff 文件；加 --keep-diff-files 可保留到 .prismflow/tmp-diffs。\n\n示例：\n  cargo run -- review once --clone-repo --clone-depth 2 --engine qwen-v1 \"qwen -y \\\"diff={patch_file} agents={agents_file} files={changed_files_file} repo={repo_dir} sha={repo_head_sha}\\\"\" --engine-prompt-file prompts/bug-only.txt --agent logic\n  cargo run -- review ad-hoc https://github.com/owner/repo/pull/123 --engine ollama:qwen2.5:1.5b \"cat {patch_file} {agents_file} {changed_files_file} | ollama run qwen2.5:1.5b\" --engine-prompt-file prompts/bug-only.txt --agent security\n  cargo run -- review daemon --ui --ui-bind 0.0.0.0:8787 --ui-token mysecret --interval-secs 30 --clone-repo --clone-depth 2 --engine qwen-v1 \"qwen -y \\\"diff: {patch_file} agents: {agents_file} files: {changed_files_file}\\\"\" --max-concurrent-repos 3 --max-concurrent-prs 6 --max-concurrent-api 12"
)]
pub struct ReviewCommand {
    #[command(subcommand)]
    pub command: ReviewSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ReviewSubcommand {
    #[command(about = "执行一次审查（会尝试提交 inline 评论或降级总结评论）")]
    Once {
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            required = true,
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用"
        )]
        engines: Vec<String>,
        #[arg(
            long,
            help = "追加到 patch_file 顶部的自定义提示词（建议 shell 模式使用）"
        )]
        engine_prompt: Option<String>,
        #[arg(
            long,
            help = "提示词文件路径，读取文件内容并追加到 patch_file 顶部（建议用于多行提示词）"
        )]
        engine_prompt_file: Option<String>,
        #[arg(
            long = "agent",
            num_args = 1..,
            action = ArgAction::Append,
            help = "启用的 Agent 名称（支持 --agent a b c 或重复 --agent a --agent b）"
        )]
        agents: Vec<String>,
        #[arg(
            long,
            default_value_t = false,
            help = "启用仓库 clone 上下文模式（可用 {repo_dir} 等占位符）"
        )]
        clone_repo: bool,
        #[arg(long, default_value = ".prismflow/repo-cache", help = "clone 缓存目录")]
        clone_workspace_dir: String,
        #[arg(long, default_value_t = 1, help = "clone/fetch 深度（最小为 1）")]
        clone_depth: usize,
        #[arg(
            long,
            default_value_t = false,
            help = "保留生成的 diff 文件（默认审查后自动删除）"
        )]
        keep_diff_files: bool,
        #[arg(long, default_value_t = 2, help = "仓库并发数")]
        max_concurrent_repos: usize,
        #[arg(long, default_value_t = 4, help = "单仓库 PR 并发数")]
        max_concurrent_prs: usize,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
        #[arg(
            long,
            default_value_t = 120,
            help = "大改动跳过阈值：文件数达到该值则直接打标签跳过"
        )]
        large_pr_max_files: u64,
        #[arg(
            long,
            default_value_t = 4000,
            help = "大改动跳过阈值：新增+删除行数达到该值则直接打标签跳过"
        )]
        large_pr_max_lines: u64,
        #[arg(
            long = "repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "仅处理指定仓库（owner/repo 或 github URL，可重复）"
        )]
        repos: Vec<String>,
        #[arg(
            long = "exclude-repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "排除指定仓库（owner/repo 或 github URL，可重复）"
        )]
        exclude_repos: Vec<String>,
    },
    #[command(about = "守护模式持续审查（按间隔轮询执行）")]
    Daemon {
        #[arg(long, default_value_t = 60, help = "轮询间隔（秒）")]
        interval_secs: u64,
        #[arg(long, default_value_t = false, help = "启用本地 HTTP 管理页面")]
        ui: bool,
        #[arg(
            long,
            default_value = "127.0.0.1:8787",
            help = "UI 监听地址，例如 127.0.0.1:8787 或 0.0.0.0:8787"
        )]
        ui_bind: String,
        #[arg(long, help = "UI 访问口令（建议在局域网访问时设置）")]
        ui_token: Option<String>,
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            required = true,
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用"
        )]
        engines: Vec<String>,
        #[arg(
            long,
            help = "追加到 patch_file 顶部的自定义提示词（建议 shell 模式使用）"
        )]
        engine_prompt: Option<String>,
        #[arg(
            long,
            help = "提示词文件路径，读取文件内容并追加到 patch_file 顶部（建议用于多行提示词）"
        )]
        engine_prompt_file: Option<String>,
        #[arg(
            long = "agent",
            num_args = 1..,
            action = ArgAction::Append,
            help = "启用的 Agent 名称（支持 --agent a b c 或重复 --agent a --agent b）"
        )]
        agents: Vec<String>,
        #[arg(
            long,
            default_value_t = false,
            help = "启用仓库 clone 上下文模式（可用 {repo_dir} 等占位符）"
        )]
        clone_repo: bool,
        #[arg(long, default_value = ".prismflow/repo-cache", help = "clone 缓存目录")]
        clone_workspace_dir: String,
        #[arg(long, default_value_t = 1, help = "clone/fetch 深度（最小为 1）")]
        clone_depth: usize,
        #[arg(
            long,
            default_value_t = false,
            help = "保留生成的 diff 文件（默认审查后自动删除）"
        )]
        keep_diff_files: bool,
        #[arg(long, default_value_t = 2, help = "仓库并发数")]
        max_concurrent_repos: usize,
        #[arg(long, default_value_t = 4, help = "单仓库 PR 并发数")]
        max_concurrent_prs: usize,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
        #[arg(
            long,
            default_value_t = 120,
            help = "大改动跳过阈值：文件数达到该值则直接打标签跳过"
        )]
        large_pr_max_files: u64,
        #[arg(
            long,
            default_value_t = 4000,
            help = "大改动跳过阈值：新增+删除行数达到该值则直接打标签跳过"
        )]
        large_pr_max_lines: u64,
        #[arg(
            long = "repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "仅处理指定仓库（owner/repo 或 github URL，可重复）"
        )]
        repos: Vec<String>,
        #[arg(
            long = "exclude-repo",
            num_args = 1..,
            action = ArgAction::Append,
            help = "排除指定仓库（owner/repo 或 github URL，可重复）"
        )]
        exclude_repos: Vec<String>,
    },
    #[command(about = "即时审查单个 PR URL（无需预先加入 repo 列表）")]
    AdHoc {
        #[arg(help = "GitHub PR 链接，例如 https://github.com/owner/repo/pull/123")]
        pr_url: String,
        #[arg(
            long = "engine",
            num_args = 2,
            action = ArgAction::Append,
            value_names = ["FINGERPRINT", "COMMAND"],
            required = true,
            help = "可重复指定：<指纹> <命令>，按 PR 轮询使用"
        )]
        engines: Vec<String>,
        #[arg(
            long,
            help = "追加到 patch_file 顶部的自定义提示词（建议 shell 模式使用）"
        )]
        engine_prompt: Option<String>,
        #[arg(
            long,
            help = "提示词文件路径，读取文件内容并追加到 patch_file 顶部（建议用于多行提示词）"
        )]
        engine_prompt_file: Option<String>,
        #[arg(
            long = "agent",
            num_args = 1..,
            action = ArgAction::Append,
            help = "启用的 Agent 名称（支持 --agent a b c 或重复 --agent a --agent b）"
        )]
        agents: Vec<String>,
        #[arg(
            long,
            default_value_t = false,
            help = "启用仓库 clone 上下文模式（可用 {repo_dir} 等占位符）"
        )]
        clone_repo: bool,
        #[arg(long, default_value = ".prismflow/repo-cache", help = "clone 缓存目录")]
        clone_workspace_dir: String,
        #[arg(long, default_value_t = 1, help = "clone/fetch 深度（最小为 1）")]
        clone_depth: usize,
        #[arg(
            long,
            default_value_t = false,
            help = "保留生成的 diff 文件（默认审查后自动删除）"
        )]
        keep_diff_files: bool,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
        #[arg(
            long,
            default_value_t = 120,
            help = "大改动跳过阈值：文件数达到该值则直接打标签跳过"
        )]
        large_pr_max_files: u64,
        #[arg(
            long,
            default_value_t = 4000,
            help = "大改动跳过阈值：新增+删除行数达到该值则直接打标签跳过"
        )]
        large_pr_max_lines: u64,
    },
    #[command(about = "清理指定 PR 上 PrismFlow 留下的评论与标签痕迹（尽力而为）")]
    Clean {
        #[arg(help = "一个或多个 GitHub PR 链接，例如 https://github.com/owner/repo/pull/123", num_args = 1..)]
        pr_urls: Vec<String>,
        #[arg(long, default_value_t = 8, help = "全局 API 并发阈值")]
        max_concurrent_api: usize,
    },
}
