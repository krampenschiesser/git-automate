# Daemon Lifecycle

## Entry point

```rust
// src/main.rs
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber)?;
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => serve(&config).await,
        Commands::Health { url, pw } => check_health(&url, &pw).await,
    }
}
```

## CLI commands

| Command | Description |
|---------|-------------|
| `serve --config <path>` | Run the daemon (default config: `git-automate.yml`) |
| `health --url <url> --pw <pw>` | Check OpenCode server health |

## `serve()` lifecycle

```
1. parse_config(config_path)
2. read GITHUB_TOKEN from env (fail fast if missing)
3. create GitHubClient
4. create ShellFn
5. build WorkflowContext → Workflow
6. run_all()              ← startup run (catch + log errors)
7. loop every 30s:
     a. run_all()        ← polling run (catch + log errors)
     b. wait for tick or SIGINT/SIGTERM
8. on signal: log and exit
```

## Startup vs polling

- **Startup run**: runs all 5 workflow steps once immediately after boot. Failures are logged but don't prevent entering the poll loop.
- **Polling run**: same 5 steps, repeated every 30 seconds.

## Signal handling

```rust
tokio::select! {
    _ = interval.tick() => { /* run checks */ }
    _ = &mut ctrl_c => {
        tracing::info!("Received shutdown signal, exiting");
        break;
    }
}
```

SIGINT and SIGTERM are handled identically — the daemon logs and exits cleanly.

## Health check

The `health` subcommand creates an `OpenCodeClient` and calls `check_health()`:

- Returns `Ok(())` and logs `healthy` if the server reports `healthy: true`.
- Returns `Err` and logs `not healthy` otherwise.

`check_health()` is tolerant: any network error, non-2xx status, or malformed response returns `false`.
