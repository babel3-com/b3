use clap::{Parser, Subcommand};

mod commands;
mod http;
mod mcp;
mod pty;
mod bridge;
mod config;
mod crypto;
mod daemon;
mod service;
mod logging;
mod skills;

#[derive(Parser)]
#[command(name = "b3", version, about = "Babel3 — voice interface for Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// First-time setup: authenticate and register agent
    Setup {
        /// Setup token (non-interactive mode, skip browser/prompts)
        #[arg(long)]
        token: Option<String>,
        /// Agent name (non-interactive mode, defaults to hostname)
        #[arg(long)]
        name: Option<String>,
    },
    /// Spawn Claude Code in a managed PTY session
    Start {
        /// Serve browser dashboard from a local directory (development mode).
        /// Points to a directory containing static/open/ with JS/CSS files.
        #[arg(long)]
        browser_dir: Option<String>,
        /// Forward /app/a/:agent/* requests to this local port.
        /// Enables serving a custom HTTP app via babel3.com with HTTPS + NAT traversal.
        #[arg(long)]
        app_port: Option<u16>,
        /// Allow unauthenticated visitors to reach /app/a/:agent/*.
        /// When omitted, only the authenticated owner can access the app URL.
        #[arg(long)]
        app_public: bool,
        /// Start daemon in background without attaching. Skips the B3_SESSION nesting
        /// check so a child agent can be launched from inside the parent's session.
        #[arg(long)]
        detached: bool,
        /// Arguments to pass through to Claude Code CLI
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        claude_args: Vec<String>,
    },
    /// Connect your terminal to the running session
    Attach,
    /// Check connection state, agent health, session info
    Status,
    /// Stop agent and Claude Code session
    Stop {
        /// Force stop even from inside a Babel3 session
        #[arg(long)]
        force: bool,
    },
    /// Re-authenticate with a new token
    Login,
    /// Remove Babel3 completely
    Uninstall,
    /// Show version
    Version,
    /// Update to latest version
    Update {
        /// Re-run install-skills and plugin migration even if already on the latest version
        #[arg(long)]
        force: bool,
    },
    /// Set or change the daemon password
    SetPassword {
        /// Set password non-interactively (for scripting / agent-manager)
        #[arg(long)]
        password: Option<String>,
    },
    /// Send a message to another agent on the mesh
    Hive {
        #[command(subcommand)]
        action: HiveAction,
    },
    /// Manage child agents (spawn, list, kill)
    Child {
        #[command(subcommand)]
        action: ChildAction,
    },
    /// Internal: run as MCP server (spawned by Claude Code, not by user)
    #[command(hide = true)]
    Mcp {
        /// Subcommand: serve (default), voice (legacy alias), session-memory (legacy alias).
        /// Bare `b3 mcp` defaults to `serve`.
        #[command(subcommand)]
        service: Option<McpService>,
    },
    /// Internal: extract embedded skills to ~/.claude/skills/ and refresh MCP configs.
    /// Called by `b3 update` after binary replacement. Not for direct use.
    #[command(hide = true)]
    InstallSkills {
        /// After refreshing skills, exec into `b3 start <args>` from this binary.
        /// Passed by `b3 start --auto-update` so the daemon launches from the new binary.
        #[arg(long, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        and_start: Vec<String>,
    },
}

#[derive(Subcommand)]
enum HiveAction {
    /// Send a message to another agent
    Send {
        /// Target agent name or ID
        agent: String,
        /// Message text
        message: String,
    },
    /// List agents belonging to the same user
    List,
    /// List conversation rooms
    Rooms,
    /// Show messages in a conversation room
    Room {
        /// Room ID (UUID)
        room_id: String,
    },
    /// Send a message to a conversation room
    Converse {
        /// Room ID (UUID)
        room_id: String,
        /// Message text
        message: String,
    },
}

#[derive(Subcommand)]
enum ChildAction {
    /// Spawn a new child agent
    Spawn {
        /// Name for the child agent
        name: String,
        /// Optional expiry duration (e.g. 1h, 30m, 2d)
        #[arg(long)]
        expires_in: Option<String>,
    },
    /// List child agents
    List {
        /// Include deleted/expired agents
        #[arg(long)]
        all: bool,
    },
    /// Kill a child agent by name or id
    Kill {
        /// Child agent name or id (hc-*)
        name_or_id: String,
    },
    /// Restore a killed or expired child agent by id (or name if still listed)
    Restore {
        /// Child agent id (hc-*) or name
        name_or_id: String,
    },
}

#[derive(Subcommand)]
enum McpService {
    /// Run the unified b3 MCP server (stdio transport)
    Serve {
        /// Config directory to use (replaces B3_CONFIG_DIR env var).
        #[arg(long)]
        config_dir: Option<String>,
        /// Working directory the daemon was started in (replaces B3_START_CWD env var).
        #[arg(long)]
        start_cwd: Option<String>,
    },
    /// Legacy alias for `b3 mcp serve` — kept so existing .mcp.json entries keep working
    #[command(hide = true)]
    Voice {
        #[arg(long)]
        config_dir: Option<String>,
        #[arg(long)]
        start_cwd: Option<String>,
    },
    /// Legacy alias for `b3 mcp serve` — kept so existing .mcp.json entries keep working
    #[command(hide = true)]
    SessionMemory,
}

fn main() -> anyhow::Result<()> {
    // Force-exit on any panic. A panicked tokio worker thread corrupts the
    // runtime's waker infrastructure — broadcast channels stop waking receivers,
    // causing silent delta delivery failures. Better to crash and restart clean
    // than run with a corrupted runtime. (Discovered 2026-04-02: TLS panic in
    // tokio-rt-worker during restart killed the local pusher's broadcast waker.)
    std::panic::set_hook(Box::new(|info| {
        eprintln!("FATAL PANIC — forcing exit to prevent corrupted runtime:\n{info}");
        std::process::exit(1);
    }));

    let cli = Cli::parse();

    // On Unix, the `start` command needs to fork BEFORE the tokio runtime starts.
    // Forking inside an async runtime inherits stale epoll fds and signal handlers,
    // causing deadlocks in the child process (especially in Docker containers).
    #[cfg(unix)]
    if let Some(Commands::Start { ref claude_args, ref browser_dir, ref app_port, ref app_public, ref detached }) = cli.command {
        if config::Config::exists() {
            let cfg = config::Config::load()?;
            if !daemon::server::is_running() {
                // Fork now — before tokio. Child enters its own runtime and runs daemon.
                // Parent gets (pid, read_fd) to wait on.
                let (pid, read_fd) = commands::start::pre_fork(cfg, claude_args.clone(), browser_dir.clone(), *app_port, *app_public)?;

                // Parent: enter tokio runtime and do the async attach (or skip if --detached)
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                return rt.block_on(commands::start::post_fork(pid, read_fd, *detached));
            }
        }
        // If config doesn't exist or daemon is already running, fall through to normal async path
    }

    // Normal async entry point (all other commands, or start with existing daemon)
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            // No subcommand: guide the user
            if config::Config::exists() {
                println!("Babel3 is set up. Run `b3 start` to launch.");
            } else {
                println!("Welcome to Babel3! Run `b3 setup` to get started.");
            }
            println!("Run `b3 --help` for all commands.");
            return Ok(());
        }
    };

    match command {
        Commands::Setup { token, name } => commands::setup::run(token, name).await,
        Commands::Start { claude_args, browser_dir, app_port, app_public, detached } => commands::start::run(claude_args, browser_dir, app_port, app_public, detached).await,
        Commands::Attach => commands::attach::run().await.map(|_| ()),
        Commands::Status => commands::status::run().await,
        Commands::Stop { force } => commands::stop::run(force).await,
        Commands::Login => commands::login::run().await,
        Commands::Uninstall => commands::uninstall::run().await,
        Commands::Version => commands::version::run().await,
        Commands::Update { force } => commands::update::run(force).await,
        Commands::SetPassword { password } => commands::set_password::run(password).await,
        Commands::Hive { action } => match action {
            HiveAction::Send { agent, message } => {
                commands::hive::send(&agent, &message).await
            }
            HiveAction::List => commands::hive::list().await,
            HiveAction::Rooms => commands::hive::rooms().await,
            HiveAction::Room { room_id } => commands::hive::room(&room_id).await,
            HiveAction::Converse { room_id, message } => {
                commands::hive::converse(&room_id, &message).await
            }
        },
        Commands::Child { action } => match action {
            ChildAction::Spawn { name, expires_in } => {
                commands::child::spawn(&name, expires_in.as_deref()).await
            }
            ChildAction::List { all } => commands::child::list(all).await,
            ChildAction::Kill { name_or_id } => commands::child::kill(&name_or_id).await,
            ChildAction::Restore { name_or_id } => commands::child::restore(&name_or_id).await,
        },
        Commands::Mcp { service } => {
            // All three variants (Serve, Voice, SessionMemory) run the same unified
            // b3 MCP server. Voice and SessionMemory are legacy aliases kept so
            // existing .mcp.json entries continue to work without reconfiguration.
            let (config_dir, start_cwd) = match service {
                Some(McpService::Serve { config_dir, start_cwd })
                | Some(McpService::Voice { config_dir, start_cwd }) => (config_dir, start_cwd),
                Some(McpService::SessionMemory) | None => (None, None),
            };
            // Init fork_state from CLI args — the MCP subprocess is exec'd fresh
            // by Claude Code, so it doesn't share the daemon's process space.
            let cwd = start_cwd
                .as_deref()
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| std::path::PathBuf::from("/"));
            let cfg_dir = config_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(config::Config::config_dir);
            daemon::fork_state::init(cfg_dir, cwd, None, None, false);
            mcp::b3::run().await
        },
        Commands::InstallSkills { and_start } => {
            let b3_bin = commands::setup::find_b3_binary();
            match commands::setup::install_skills() {
                Ok(path) => println!("  Skills refreshed at {}", path.display()),
                Err(e) => println!("  ⚠ Could not refresh skills: {e}"),
            }
            if let Err(e) = commands::setup::register_voice_mcp(&b3_bin) {
                println!("  ⚠ Could not refresh .mcp.json: {e}");
            }
            if let Err(e) = commands::setup::register_codex_mcp(&b3_bin) {
                tracing::debug!("Codex MCP refresh (non-fatal): {e}");
            }
            let _ = commands::setup::uninstall_claude_plugin_if_present();
            commands::setup::remove_legacy_session_memory_entry();
            // Chain into `b3 start <args>` if requested (auto-update path).
            // exec() replaces this process — daemon launches from the new binary.
            if !and_start.is_empty() {
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    let current_exe = std::env::current_exe().unwrap_or(b3_bin);
                    let err = std::process::Command::new(&current_exe)
                        .arg("start")
                        .args(&and_start)
                        .exec();
                    eprintln!("  ⚠ Could not exec into b3 start: {err}");
                }
                #[cfg(windows)]
                {
                    let current_exe = std::env::current_exe().unwrap_or(b3_bin);
                    let _ = std::process::Command::new(&current_exe)
                        .arg("start")
                        .args(&and_start)
                        .spawn();
                }
            }
            Ok(())
        },
    }
}
