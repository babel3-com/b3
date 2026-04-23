//! b3 stop — Stop the agent daemon and Claude Code session.
//!
//! Connects to the daemon socket and sends a Stop message.
//! The daemon will kill the PTY, close the tunnel, and exit.
//!
//! Service lifecycle design:
//! - `b3 stop`      → stops daemon AND deregisters boot service. After reboot, b3 stays dead.
//! - `b3 start`     → starts daemon AND registers boot service. After reboot, b3 comes back.
//! - `b3 uninstall` → nuclear option, removes everything including config.
//!
//! systemd Restart=on-failure compatibility: the daemon calls std::process::exit(0)
//! on clean stop (server.rs), so systemd will NOT restart it after `b3 stop`.
//! Restart only fires on non-zero exit (crash/OOM), which is the intended behavior.

use crate::daemon::ipc::IpcStream;
use crate::daemon::protocol::{self, Message};
use crate::daemon::server;

pub async fn run(force: bool) -> anyhow::Result<()> {
    // Refuse to stop from inside a Babel3 session unless --force is passed.
    // Agents won't pass --force. Humans who really mean it can.
    if std::env::var("B3_SESSION").is_ok() && !force {
        anyhow::bail!(
            "You're inside a Babel3 session. Stopping will kill this session.\n\
             \n\
             If you really want to stop, run:\n\
             \n\
             \x1b[1m  b3 stop --force\x1b[0m\n\
             \n\
             Or use the restart_session MCP tool to restart cleanly."
        );
    }

    if !server::is_running() {
        println!("No Babel3 daemon running.");
        // Clean up stale files just in case
        crate::daemon::ipc::cleanup_endpoint();
        let _ = std::fs::remove_file(server::pid_path());
        if let Err(e) = crate::service::uninstall() {
            tracing::debug!("service uninstall: {e}");
        }
        return Ok(());
    }

    if !crate::daemon::ipc::endpoint_exists() {
        // PID file exists but no IPC endpoint — force kill
        println!("Daemon IPC endpoint missing. Terminating daemon process...");
        if let Ok(contents) = std::fs::read_to_string(server::pid_path()) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                server::kill_process(pid);
                println!("Terminated PID {pid}.");
            }
        }
        let _ = std::fs::remove_file(server::pid_path());
        if let Err(e) = crate::service::uninstall() {
            tracing::debug!("service uninstall: {e}");
        }
        return Ok(());
    }

    // Connect and send Stop
    println!("Stopping Babel3 daemon...");
    match IpcStream::connect().await {
        Ok(stream) => {
            let (_reader, mut writer) = stream.into_split();
            protocol::write_message(&mut writer, &Message::Stop).await?;

            // Wait for daemon to exit (up to 8 seconds — daemon has a 5s force-exit timer)
            for _ in 0..32 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if !server::is_running() {
                    println!("Daemon stopped.");
                    if let Err(e) = crate::service::uninstall() {
                        tracing::debug!("service uninstall: {e}");
                    }
                    return Ok(());
                }
            }

            // Daemon still alive after 8 seconds — force kill by PID
            println!("Daemon did not exit cleanly. Force-killing...");
            if let Ok(contents) = std::fs::read_to_string(server::pid_path()) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    server::kill_process(pid);
                    // Wait briefly for process to die
                    for _ in 0..8 {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        if !server::is_running() {
                            break;
                        }
                    }
                    if server::is_running() {
                        // SIGTERM didn't work — SIGKILL
                        #[cfg(unix)]
                        unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
            let _ = std::fs::remove_file(server::pid_path());
            crate::daemon::ipc::cleanup_endpoint();
            println!("Daemon stopped.");
            if let Err(e) = crate::service::uninstall() {
                tracing::debug!("service uninstall: {e}");
            }
        }
        Err(e) => {
            println!("Could not connect to daemon: {e}");
            println!("Cleaning up stale files...");
            crate::daemon::ipc::cleanup_endpoint();
            let _ = std::fs::remove_file(server::pid_path());
            // Do NOT deregister the service here — IPC failure doesn't mean the
            // daemon is stopped (it may be alive but temporarily unreachable).
            // Removing boot persistence when we don't know if stop succeeded
            // would leave the user with no daemon and no auto-restart.
        }
    }

    Ok(())
}
