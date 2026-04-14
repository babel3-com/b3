//! b3 stop — Stop the agent daemon and Claude Code session.
//!
//! Connects to the daemon socket and sends a Stop message.
//! The daemon will kill the PTY, close the tunnel, and exit.

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
        }
        Err(e) => {
            println!("Could not connect to daemon: {e}");
            println!("Cleaning up stale files...");
            crate::daemon::ipc::cleanup_endpoint();
            let _ = std::fs::remove_file(server::pid_path());
        }
    }

    Ok(())
}
