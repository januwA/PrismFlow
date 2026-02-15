# PrismFlow

PrismFlow is a workflow automation tool that helps manage repository workflows and integrates with GitHub for automated reviews and actions.

## Features

- Repository management
- Agent integration for automated tasks
- GitHub pull request reviews
- Customizable workflows

## Usage

See the CLI help for detailed usage instructions:

```bash
cargo run -- --help
```

## Configuration

The application uses a configuration file located at the system config directory under `pr-reviewer/repos.json`.

On Windows, this is typically located at:
`C:\Users\<username>\AppData\Roaming\pr-reviewer\repos.json`