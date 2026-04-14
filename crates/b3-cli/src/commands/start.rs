//! b3 start — Start the Babel3 daemon and auto-attach.
//!
//! Flow (Unix):
//!   1. Load config (bail if not set up)
//!   2. Check if daemon is already running → just attach
//!   3. Fork BEFORE tokio starts: child calls setsid() and enters its own runtime
//!   4. Parent waits for "ready" signal via pipe, then auto-attaches
//!   5. On detach (Ctrl+B d): parent exits, daemon keeps running
//!   6. On quit (Ctrl+B q): sends stop signal, daemon shuts down
//!
//! Flow (Windows):
//!   Same as before — daemon runs in-process (no fork).

use crate::commands::attach;
use crate::config::Config;
use crate::daemon::server;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn run(claude_args: Vec<String>, browser_dir: Option<String>) -> anyhow::Result<()> {
    // 0. Refuse to nest — detect if we're inside an existing Babel3 session.
    //    B3_SESSION is set by the daemon before spawning child processes.
    //    Same pattern as tmux's TMUX env var.
    if std::env::var("B3_SESSION").is_ok() {
        anyhow::bail!(
            "Cannot start Babel3 inside a Babel3 session.\n\
             You're already inside a running Babel3 daemon.\n\
             To restart, use the restart_session MCP tool, or from a separate terminal:\n\
             \n\
             \x1b[1m  b3 stop && b3 start\x1b[0m"
        );
    }

    // 1. Load config — auto-run setup if not configured
    if !Config::exists() {
        eprintln!("\x1b[36mFirst-time setup — let's get you registered.\x1b[0m");
        eprintln!();
        crate::commands::setup::run(None, None).await?;
        // setup saves config — if we get here, it succeeded
        if !Config::exists() {
            anyhow::bail!("Setup completed but config not found. Run `b3 setup` manually.");
        }
    }
    let config = Config::load()?;

    // 1b. Self-healing MCP registration — ensure .mcp.json and ~/.codex/config.toml
    //     have the b3 entry. Runs on every start so it doesn't matter what order
    //     tools are installed in.
    {
        let b3_bin = crate::commands::setup::find_b3_binary();
        if let Err(e) = crate::commands::setup::register_voice_mcp(&b3_bin) {
            tracing::debug!("MCP self-heal (.mcp.json): {e}");
        }
        if let Err(e) = crate::commands::setup::register_codex_mcp(&b3_bin) {
            tracing::debug!("MCP self-heal (codex): {e}");
        }
    }

    // 2. Check if daemon is already running — just attach
    if server::is_running() {
        if has_controlling_terminal() {
            eprintln!("Babel3 daemon is already running. Attaching...");
            attach::run_with_banner(&startup_banner(&config)).await?;
        } else {
            eprintln!("Babel3 daemon is already running.");
            eprintln!("No controlling terminal — skipping attach.");
            eprintln!("Stop with: b3 stop");
        }
        return Ok(());
    }

    // 3. Start daemon
    #[cfg(unix)]
    {
        start_in_process(config, claude_args, browser_dir).await?;
    }

    #[cfg(windows)]
    {
        start_in_process(config, claude_args, browser_dir).await?;
    }

    Ok(())
}

/// Build the welcome banner shown inside the attached terminal.
/// Uses \r\n because the terminal is already in raw mode when this prints.
fn startup_banner(config: &Config) -> String {
    // Always show production dashboard URL (babel3.com), even for dev agents.
    // Dev agents still use the production browser for testing.
    let dashboard = production_dashboard_url(config);
    format!(
        "\x1b[36m\x1b[1m\
Babel3 v{VERSION}\x1b[0m\r\n\
\r\n\
\x1b[90m  This is a persistent session. Claude Code keeps running even\r\n\
  when you disconnect — your tools, voice, and dashboard stay live.\x1b[0m\r\n\
\r\n\
  \x1b[33mCtrl+B d\x1b[0m   Detach (daemon keeps running in background)\r\n\
  \x1b[33mCtrl+B q\x1b[0m   Quit (stop the daemon)\r\n\
  \x1b[33mb3 stop\x1b[0m  Stop from another terminal\r\n\
\r\n\
  \x1b[34mDashboard:\x1b[0m {dashboard}\r\n\
\r\n\
\x1b[90m─────────────────────────────────────────────────\x1b[0m\r\n"
    )
}

/// Always return the production dashboard URL (babel3.com/a/{agent}).
/// The config may point to dev (hey-code.ai) but users always check
/// the production dashboard, even when running a dev server.
fn production_dashboard_url(config: &Config) -> String {
    // Extract agent name from web_url (e.g., "https://hey-code.ai/a/zara" → "zara")
    // or from agent_email (e.g., "zara@hey-code.ai" → "zara")
    let agent_name = config.web_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            config.agent_email.split('@').next().unwrap_or("agent")
        });
    format!("https://babel3.com/a/{}", agent_name)
}

/// Pre-fork entry point. Called from main() BEFORE the tokio runtime starts.
/// Returns Ok(Some(read_fd)) for the parent (with pipe to wait on),
/// or never returns for the child (enters its own runtime and runs the daemon).
///
/// This must be called before #[tokio::main] because forking inside an async
/// runtime inherits stale epoll fds, signal handlers, and thread pool state
/// from the parent, which can cause deadlocks or hangs in the child.
#[cfg(unix)]
pub fn pre_fork(config: Config, claude_args: Vec<String>, browser_dir: Option<String>) -> anyhow::Result<(i32, i32)> {
    // Save cwd BEFORE fork — the PTY will start in this directory instead of $HOME
    if let Ok(cwd) = std::env::current_dir() {
        std::env::set_var("B3_START_CWD", cwd);
    }
    // Pass browser_dir across fork boundary via env var
    if let Some(ref dir) = browser_dir {
        std::env::set_var("B3_BROWSER_DIR", dir);
    }
    // Create a pipe for the child to signal "ready" to the parent.
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("pipe() failed");
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork() failed");
    }

    if pid == 0 {
        // ── Child process (becomes the daemon) ──
        unsafe {
            libc::close(read_fd);
            libc::setsid(); // New session — immune to SIGHUP from terminal
            libc::signal(libc::SIGHUP, libc::SIG_IGN);

            // Redirect stdin to /dev/null
            let devnull = libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_RDWR);
            if devnull >= 0 {
                libc::dup2(devnull, 0); // stdin → /dev/null
                if devnull > 2 {
                    libc::close(devnull);
                }
            }

            // Redirect stdout/stderr to an early log file (visible even before
            // tracing is initialized, catches panics in tokio runtime build, etc.)
            let early_log = libc::open(
                b"/tmp/b3-daemon-early.log\0".as_ptr() as *const _,
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            );
            if early_log >= 0 {
                libc::dup2(early_log, 1); // stdout → early log
                libc::dup2(early_log, 2); // stderr → early log
                if early_log > 2 {
                    libc::close(early_log);
                }
            }
        }

        // Build a FRESH tokio runtime — no inherited state from parent
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("DAEMON FATAL: Failed to build tokio runtime: {e}");
                unsafe {
                    libc::write(write_fd, b"F".as_ptr() as *const libc::c_void, 1);
                    libc::close(write_fd);
                }
                std::process::exit(1);
            }
        };

        rt.block_on(async move {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

            let daemon_result = tokio::spawn(async move {
                server::run_daemon(config, false, claude_args, Some(ready_tx)).await
            });

            if ready_rx.await.is_ok() {
                unsafe {
                    libc::write(write_fd, b"R".as_ptr() as *const libc::c_void, 1);
                    libc::close(write_fd);
                }
            } else {
                // ready_tx was dropped → run_daemon returned Err before sending ready
                eprintln!("DAEMON: run_daemon failed before signaling ready");
                unsafe {
                    libc::write(write_fd, b"F".as_ptr() as *const libc::c_void, 1);
                    libc::close(write_fd);
                }
                // Still await daemon_result to log the actual error
                match daemon_result.await {
                    Ok(Err(e)) => eprintln!("DAEMON ERROR: {e:?}"),
                    Err(e) => eprintln!("DAEMON PANIC: {e}"),
                    Ok(Ok(())) => eprintln!("DAEMON: exited Ok but didn't signal ready?"),
                }
                std::process::exit(1);
            }

            match daemon_result.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("DAEMON ERROR (post-ready): {e:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("DAEMON PANIC (post-ready): {e}");
                    std::process::exit(1);
                }
            }
        });

        std::process::exit(0);
    }

    // ── Parent process ──
    unsafe { libc::close(write_fd) };

    Ok((pid, read_fd))
}

/// Parent-side async: wait for daemon ready signal, then attach.
#[cfg(unix)]
pub async fn post_fork(pid: i32, read_fd: i32) -> anyhow::Result<()> {
    use std::os::unix::io::FromRawFd;

    // Set pipe to non-blocking so we can timeout if child dies without writing
    unsafe {
        let flags = libc::fcntl(read_fd, libc::F_GETFL);
        libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let pipe_read = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 1];
    let mut n = 0usize;

    // Poll the pipe with timeout: up to 30s for daemon to signal ready/failed
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match std::io::Read::read(&mut &pipe_read, &mut buf) {
            Ok(bytes) => {
                n = bytes;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    // Check if child process is still alive
                    let child_alive = unsafe { libc::kill(pid, 0) == 0 };
                    if child_alive {
                        anyhow::bail!(
                            "Daemon timed out (30s) without signaling ready.\n\
                             Check /tmp/b3-daemon.log and /tmp/b3-daemon-early.log"
                        );
                    } else {
                        anyhow::bail!(
                            "Daemon child process (PID {pid}) died before signaling ready.\n\
                             Check /tmp/b3-daemon-early.log for pre-runtime errors,\n\
                             and /tmp/b3-daemon.log for runtime errors."
                        );
                    }
                }
                // Check if child is still alive while waiting
                let child_alive = unsafe { libc::kill(pid, 0) == 0 };
                if !child_alive {
                    anyhow::bail!(
                        "Daemon child process (PID {pid}) exited unexpectedly.\n\
                         Check /tmp/b3-daemon-early.log for pre-runtime errors,\n\
                         and /tmp/b3-daemon.log for runtime errors."
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => {
                anyhow::bail!("Failed to read daemon ready signal: {e}");
            }
        }
    }

    if n == 0 || buf[0] != b'R' {
        anyhow::bail!(
            "Daemon failed to start (signaled failure).\n\
             Check /tmp/b3-daemon.log for details."
        );
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Check if we have a controlling terminal before trying to attach.
    // In Docker (without -t flag), /dev/tty doesn't exist → ENXIO.
    // The daemon is running fine; we just can't attach interactively.
    if !has_controlling_terminal() {
        eprintln!("Babel3 daemon started successfully (PID {pid}).");
        eprintln!("No controlling terminal detected — skipping interactive attach.");
        eprintln!("Dashboard: check tunnel URL in /tmp/b3-daemon.log");
        eprintln!("Stop with: b3 stop");
        return Ok(());
    }

    let config = Config::load().unwrap_or_else(|_| Config {
        agent_id: String::new(), agent_email: String::new(), api_key: String::new(),
        api_url: String::new(), wg_address: String::new(), relay_endpoint: String::new(),
        relay_public_key: String::new(), push_interval_ms: 0,
        web_url: b3_common::public_url(), b3_version: String::new(), servers: Vec::new(),
    });
    let reason = attach::run_with_banner(&startup_banner(&config)).await?;

    match reason {
        attach::ExitReason::Detached => {
            eprintln!("Daemon still running (PID {pid}). Reattach with: b3 attach");
            eprintln!("Stop with: b3 stop");
        }
        attach::ExitReason::Stopped => {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        _ => {}
    }

    Ok(())
}

/// In-process daemon (used on all platforms as fallback, and on Windows always).
async fn start_in_process(config: Config, claude_args: Vec<String>, browser_dir: Option<String>) -> anyhow::Result<()> {
    // Save cwd — the PTY will start in this directory instead of $HOME
    if let Ok(cwd) = std::env::current_dir() {
        std::env::set_var("B3_START_CWD", cwd);
    }
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    let banner = startup_banner(&config);

    // Print startup info to stderr (survives attach screen-clear)
    eprintln!("\x1b[36mStarting Babel3 v{VERSION}...\x1b[0m");
    eprintln!("  Agent: {}", config.agent_email);
    eprintln!("  Dashboard: {}", production_dashboard_url(&config));

    // Pass browser_dir via env var so run_daemon can pick it up
    if let Some(ref dir) = browser_dir {
        std::env::set_var("B3_BROWSER_DIR", dir);
    }

    let config_for_daemon = config.clone();
    let daemon_handle = tokio::spawn(async move {
        // foreground=false — we handle status printing ourselves via stderr
        server::run_daemon(config_for_daemon, false, claude_args, Some(ready_tx)).await
    });

    if ready_rx.await.is_err() {
        eprintln!("\x1b[31m  ✗ Daemon failed to start\x1b[0m");
        match daemon_handle.await {
            Ok(Err(e)) => return Err(e),
            Err(e) => anyhow::bail!("Daemon task panicked: {e}"),
            Ok(Ok(())) => return Ok(()),
        }
    }

    eprintln!("\x1b[32m  ✓ Daemon ready\x1b[0m");

    // Check if we have a controlling terminal before trying to attach.
    if !has_controlling_terminal() {
        eprintln!("No controlling terminal detected — skipping interactive attach.");
        eprintln!("Dashboard: {}", production_dashboard_url(&config));
        eprintln!("Stop with: b3 stop");
        // Keep daemon running — wait for it
        match daemon_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("Daemon error: {e}"),
            Err(e) => eprintln!("Daemon task panicked: {e}"),
        }
        return Ok(());
    }

    let reason = attach::run_with_banner(&banner).await?;

    match reason {
        attach::ExitReason::Detached => {
            eprintln!("Daemon still running. Reattach with: b3 attach");
            eprintln!("Stop with: b3 stop");
            match daemon_handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("Daemon error: {e}"),
                Err(e) => eprintln!("Daemon task panicked: {e}"),
            }
        }
        _ => {
            daemon_handle.abort();
        }
    }

    Ok(())
}

/// Check if this process has a controlling terminal.
/// Returns false in Docker containers started without -t, headless services, etc.
fn has_controlling_terminal() -> bool {
    #[cfg(unix)]
    {
        let fd = unsafe { libc::open(b"/dev/tty\0".as_ptr() as *const _, libc::O_RDWR) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            true
        } else {
            false
        }
    }
    #[cfg(windows)]
    {
        // Windows always has a console (or can allocate one)
        true
    }
}
