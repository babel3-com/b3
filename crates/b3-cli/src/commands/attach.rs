//! b3 attach — Connect user's terminal to the running PTY session via Unix socket.
//!
//! Flow:
//!   1. Connect to daemon socket (~/.b3/b3.sock)
//!   2. Receive Welcome message with PTY dimensions
//!   3. Put terminal in raw mode
//!   4. Forward: daemon output → stdout, stdin → daemon input
//!   5. Detect Ctrl+B d → send Detach, restore terminal, exit
//!   6. Forward SIGWINCH → Resize message
//!   7. On daemon disconnect or PTY exit → restore terminal, exit

use std::io::{Read, Write};

use crate::daemon::ipc::IpcStream;
use crate::daemon::protocol::{self, Message};
use crate::daemon::server;

/// Signal sent from the stdin thread to the main task on Ctrl+B d / Ctrl+B q.
#[derive(Debug)]
enum StdinSignal {
    Detach,
    Stop,
}

pub async fn run() -> anyhow::Result<ExitReason> {
    run_with_banner().await
}

pub async fn run_with_banner() -> anyhow::Result<ExitReason> {
    // Check if daemon is running
    if !server::is_running() {
        anyhow::bail!(
            "No Babel3 daemon running.\n\
             Start one with: b3 start"
        );
    }

    if !crate::daemon::ipc::endpoint_exists() {
        anyhow::bail!(
            "Daemon PID file exists but IPC endpoint is missing.\n\
             The daemon may have crashed. Try: b3 stop && b3 start"
        );
    }

    // Connect to daemon
    let stream = IpcStream::connect().await?;
    let (mut reader, writer) = stream.into_split();

    // Read Welcome
    let welcome = protocol::read_message(&mut reader).await?;
    let (_rows, _cols) = match welcome {
        Message::Welcome { rows, cols } => (rows, cols),
        other => {
            anyhow::bail!("Expected Welcome message, got: {other:?}");
        }
    };

    // Put terminal in raw mode
    crossterm::terminal::enable_raw_mode()?;

    // Ensure we restore terminal on any exit path
    let _raw_guard = RawModeGuard;

    print!("\x1b[2J\x1b[H"); // Clear screen
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Spawn output reader task (daemon → stdout)
    let output_task = tokio::spawn(async move {
        let mut stdout = std::io::stdout();
        loop {
            match protocol::read_message(&mut reader).await {
                Ok(Message::Output(data)) => {
                    let _ = stdout.write_all(&data);
                    let _ = stdout.flush();
                }
                Ok(Message::Exited(code)) => {
                    let _ = crossterm::terminal::disable_raw_mode();
                    eprintln!("\nClaude Code exited (code {code}).");
                    return ExitReason::ChildExited;
                }
                Err(_) => {
                    return ExitReason::Disconnected;
                }
                Ok(_) => {
                    // Ignore unexpected messages
                }
            }
        }
    });

    // Spawn stdin reader thread (stdin → daemon)
    // Must use OS thread because stdin.read() is blocking
    let rt = tokio::runtime::Handle::current();
    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<StdinSignal>();

    let writer_handle = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    // Send a newline to the PTY so Claude Code redraws its prompt after the banner.
    // Without this, the prompt was already printed before attach connected.
    {
        let mut w = writer_handle.lock().await;
        let _ = protocol::write_message(&mut *w, &Message::Input(b"\n".to_vec())).await;
    }

    let writer_for_stdin = writer_handle.clone();

    std::thread::Builder::new()
        .name("attach-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            let mut buf = [0u8; 1024];
            let mut saw_prefix = false; // Ctrl+B prefix state

            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &buf[..n];

                        // Check for Ctrl+B sequences
                        for &byte in data {
                            if saw_prefix {
                                saw_prefix = false;
                                if byte == b'd' || byte == b'D' {
                                    // Ctrl+B d → Detach (daemon keeps running)
                                    let wh = writer_for_stdin.clone();
                                    rt.block_on(async {
                                        let mut w = wh.lock().await;
                                        let _ = protocol::write_message(
                                            &mut *w,
                                            &Message::Detach,
                                        )
                                        .await;
                                    });
                                    let _ = signal_tx.send(StdinSignal::Detach);
                                    return;
                                }
                                if byte == b'q' || byte == b'Q' {
                                    // Ctrl+B q → Quit (stop daemon)
                                    let wh = writer_for_stdin.clone();
                                    rt.block_on(async {
                                        let mut w = wh.lock().await;
                                        let _ = protocol::write_message(
                                            &mut *w,
                                            &Message::Stop,
                                        )
                                        .await;
                                    });
                                    let _ = signal_tx.send(StdinSignal::Stop);
                                    return;
                                }
                                // Not a recognized sequence — send the Ctrl+B and this byte
                                let prefix_and_byte = vec![0x02, byte];
                                let wh = writer_for_stdin.clone();
                                rt.block_on(async {
                                    let mut w = wh.lock().await;
                                    let _ = protocol::write_message(
                                        &mut *w,
                                        &Message::Input(prefix_and_byte),
                                    )
                                    .await;
                                });
                                continue;
                            }

                            if byte == 0x02 {
                                // Ctrl+B — start prefix
                                saw_prefix = true;
                                continue;
                            }
                        }

                        // If we consumed all bytes as prefix handling, skip send
                        if saw_prefix {
                            continue;
                        }

                        // Send raw input to daemon
                        let wh = writer_for_stdin.clone();
                        let input = data.to_vec();
                        rt.block_on(async {
                            let mut w = wh.lock().await;
                            let _ = protocol::write_message(
                                &mut *w,
                                &Message::Input(input),
                            )
                            .await;
                        });
                    }
                    Err(_) => break,
                }
            }
        })?;

    // Send initial terminal size immediately on attach.
    // The PTY may have been created with a different size (from the daemon's terminal
    // at startup, or from a previous client). Without this, bash renders with wrong
    // COLUMNS and multi-line commands leave ghost lines when navigating history.
    {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let mut w = writer_handle.lock().await;
        let _ = protocol::write_message(
            &mut *w,
            &Message::Resize { rows, cols },
        ).await;
    }

    // Spawn resize watcher
    let writer_for_resize = writer_handle.clone();
    let resize_task = tokio::spawn(async move {
        let mut last_size = crossterm::terminal::size().unwrap_or((80, 24));
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if let Ok(size) = crossterm::terminal::size() {
                if size != last_size {
                    last_size = size;
                    let (cols, rows) = size;
                    let mut w = writer_for_resize.lock().await;
                    let _ = protocol::write_message(
                        &mut *w,
                        &Message::Resize { rows, cols },
                    )
                    .await;
                }
            }
        }
    });

    // Wait for either: output task ends or stdin signal (detach/stop)
    let reason = tokio::select! {
        result = output_task => {
            match result {
                Ok(ExitReason::ChildExited) => {
                    // Terminal already restored in the task
                    ExitReason::ChildExited
                }
                Ok(ExitReason::Disconnected) => {
                    let _ = crossterm::terminal::disable_raw_mode();
                    eprintln!("\nDaemon disconnected.");
                    ExitReason::Disconnected
                }
                _ => {
                    let _ = crossterm::terminal::disable_raw_mode();
                    eprintln!("\nConnection error.");
                    ExitReason::Disconnected
                }
            }
        }
        result = signal_rx => {
            let _ = crossterm::terminal::disable_raw_mode();
            match result {
                Ok(StdinSignal::Detach) => {
                    eprintln!("\nDetached from session. (Ctrl+B d)");
                    eprintln!("Session is still running. Reattach with: b3 attach");
                    ExitReason::Detached
                }
                Ok(StdinSignal::Stop) => {
                    eprintln!("\nStopping daemon... (Ctrl+B q)");
                    ExitReason::Stopped
                }
                Err(_) => {
                    // Stdin thread exited without sending — shouldn't happen in normal flow
                    eprintln!("\nAttach session ended unexpectedly.");
                    ExitReason::Disconnected
                }
            }
        }
    };

    resize_task.abort();
    Ok(reason)
}

/// Why the attach session ended. Public so `start.rs` can distinguish
/// detach (daemon keeps running) from exit (daemon should stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    ChildExited,
    Disconnected,
    Detached,
    Stopped,
}

/// RAII guard to restore terminal mode.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
