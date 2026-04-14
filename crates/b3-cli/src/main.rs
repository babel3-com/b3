use clap::{Parser, Subcommand};

mod commands;
mod http;
mod mcp;
mod pty;
mod mesh;
mod bridge;
mod config;
mod crypto;
mod daemon;
mod service;
mod logging;

#[derive(Parser)]
#[command(name = "b3", version, about = "Babel3 — voice interface + mesh network for Claude Code")]
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
    Update,
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
    /// Internal: run as MCP server (spawned by Claude Code, not by user)
    #[command(hide = true)]
    Mcp {
        #[command(subcommand)]
        service: McpService,
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
enum McpService {
    /// Voice MCP server (stdio transport)
    Voice,
    /// Session memory MCP server (stdio transport)
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
    if let Some(Commands::Start { ref claude_args, ref browser_dir }) = cli.command {
        if config::Config::exists() {
            let cfg = config::Config::load()?;
            if !daemon::server::is_running() {
                // Fork now — before tokio. Child enters its own runtime and runs daemon.
                // Parent gets (pid, read_fd) to wait on.
                let (pid, read_fd) = commands::start::pre_fork(cfg, claude_args.clone(), browser_dir.clone())?;

                // Parent: enter tokio runtime and do the async attach
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                return rt.block_on(commands::start::post_fork(pid, read_fd));
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
        Commands::Start { claude_args, browser_dir } => commands::start::run(claude_args, browser_dir).await,
        Commands::Attach => commands::attach::run().await.map(|_| ()),
        Commands::Status => commands::status::run().await,
        Commands::Stop { force } => commands::stop::run(force).await,
        Commands::Login => commands::login::run().await,
        Commands::Uninstall => commands::uninstall::run().await,
        Commands::Version => commands::version::run().await,
        Commands::Update => commands::update::run().await,
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
        Commands::Mcp { service } => match service {
            McpService::Voice => mcp::voice::run().await,
            McpService::SessionMemory => mcp::session_memory::run().await,
        },
    }
}
