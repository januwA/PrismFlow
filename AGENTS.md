# Repository Guidelines

## Project Structure & Module Organization
PrismFlow is a Rust CLI-first project using a layered architecture under `src/`:

- `src/domain/`: core entities, errors, and ports (traits such as `GitHubRepository`, `ShellAdapter`, `ProcessManager`).
- `src/application/`: use cases and workflow orchestration (`review_workflow`, `ci_workflow`, `auth_manager`, task context).
- `src/infrastructure/`: adapter implementations (GitHub API, shell/process, local config, token providers).
- `src/interface/`: CLI/web entry handling (`cli_handlers`, `web`, command parsing).
- `src/main.rs`: composition root; wires concrete adapters into application services.

Tests are primarily colocated as unit tests inside source files (`#[cfg(test)]`).

## Build, Test, and Development Commands
- `cargo build`: compile the project.
- `cargo run -- --help`: inspect CLI options.
- `cargo run -- review once ...`: run a single review workflow.
- `cargo run -- ci daemon ...`: run CI analysis daemon mode.
- `cargo test -q`: run all tests quietly.
- `cargo fmt`: format code (required before commit).

## Coding Style & Naming Conventions
- Use standard Rust formatting (`cargo fmt`) and idioms.
- Naming:
  - `snake_case` for functions/modules/files.
  - `PascalCase` for structs/enums/traits.
  - Clear suffixes for adapters and ports (e.g., `*Repository`, `*Adapter`, `*Provider`).
- Keep dependency direction strict: `interface/infrastructure -> application/domain`; avoid `application -> infrastructure`.

## Testing Guidelines
- Prefer focused unit tests near the code they validate.
- Name tests by behavior, e.g., `run_once_respects_cancelled_task_context`.
- Add regression tests for cancellation, process cleanup, and workflow edge cases when changing those paths.
- Run `cargo test -q` before opening a PR.

## Commit & Pull Request Guidelines
- Follow Conventional Commit style when possible:
  - `feat(...)`, `refactor(...)`, `fix(...)`, `docs(...)`.
  - Example: `refactor(auth): decouple AuthManager via TokenWriter port`.
- Keep commits scoped to one logical change.
- PRs should include:
  - What changed and why.
  - Risk/rollback notes for workflow or cancellation logic.
  - Test evidence (command + result), e.g., `cargo test -q` passed.
