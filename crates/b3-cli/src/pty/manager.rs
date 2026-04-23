//! PTY lifecycle: spawn Claude Code, read output, write input, handle resize.
//!
//! Uses portable-pty for cross-platform PTY management.
//! - Spawns `claude` in a PTY with configurable dimensions
//! - Reads output continuously via a background thread (PTY I/O is blocking)
//! - Broadcasts output to multiple consumers (user terminal + session pusher)
//! - Provides a writer handle for input injection (transcriptions, hive messages)
//! - Forwards terminal resize via SIGWINCH

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// Thread-safe handle for resizing the PTY from any context.
pub type PtyResizer = Arc<std::sync::Mutex<Box<dyn MasterPty + Send>>>;

/// Manages a Claude Code process running inside a PTY.
pub struct PtyManager {
    /// Child process handle (for wait/kill).
    child: Box<dyn Child + Send + Sync>,
    /// Master PTY handle (for resize), wrapped for sharing.
    master_resizer: PtyResizer,
    /// Broadcast sender — subscribe to get PTY output.
    output_tx: broadcast::Sender<Vec<u8>>,
    /// Writer for injecting input into the PTY.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl PtyManager {
    /// Spawn `claude` in a new PTY with the given dimensions.
    ///
    /// Returns a PtyManager. Call `subscribe_output()` to get output receivers
    /// for the pusher, stdout forwarder, etc.
    pub fn spawn(rows: u16, cols: u16) -> anyhow::Result<Self> {
        Self::spawn_command("claude", &[], rows, cols)
    }

    /// Spawn `claude` with extra CLI arguments passed through.
    pub fn spawn_with_args(rows: u16, cols: u16, args: &[&str]) -> anyhow::Result<Self> {
        // If first arg looks like a program name (no dash prefix), use it as the command.
        // e.g. `b3 start -- claude` spawns claude, `b3 start -- python` spawns python.
        if let Some(first) = args.first() {
            if !first.starts_with('-') {
                return Self::spawn_command(first, &args[1..], rows, cols);
            }
        }
        // Default: bash shell. User calls `claude` from inside when ready.
        Self::spawn_command("bash", args, rows, cols)
    }

    /// Spawn a custom command in a new PTY.
    ///
    /// Used by `spawn()` for production (claude) and by tests for test commands.
    pub fn spawn_command(program: &str, args: &[&str], rows: u16, cols: u16) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        // Set working directory: use fork_state start_cwd (set at `b3 start` time),
        // fall back to current process cwd. Without this, portable-pty defaults to $HOME.
        if let Some(start_cwd) = crate::daemon::fork_state::start_cwd() {
            if start_cwd.is_dir() {
                cmd.cwd(start_cwd);
            }
        } else if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        // Pass through useful env vars
        for var in &["TERM", "HOME", "PATH", "USER", "SHELL", "LANG", "LC_ALL"] {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        // Ensure TERM is color-capable (hosted sessions inherit TERM=dumb from
        // the daemon process which runs without a TTY)
        let term = std::env::var("TERM").unwrap_or_default();
        if term.is_empty() || term == "dumb" {
            cmd.env("TERM", "xterm-256color");
        }

        let child = pair.slave.spawn_command(cmd)?;

        // Drop slave — we only interact via the master side
        drop(pair.slave);

        // Get reader and writer from master
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        // Broadcast channel for PTY output — multiple consumers can subscribe.
        // 4096 slots: Claude's welcome screen generates hundreds of chunks during
        // the 3-second startup wait while subscribers are idle. At 512 slots, the
        // buffer wraps around and invalidates subscriber positions, causing permanent
        // Lagged loops that silently kill the local pusher and rebroadcast tasks.
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(4096);
        let tx = output_tx.clone();

        // Spawn a thread to read PTY output (blocking I/O — can't use async)
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF — child exited
                        Ok(n) => {
                            // Broadcast to all subscribers. If no subscribers,
                            // the send fails silently (data is just dropped).
                            let _ = tx.send(buf[..n].to_vec());
                        }
                        Err(e) => {
                            tracing::debug!("PTY read error: {e}");
                            break;
                        }
                    }
                }
                tracing::info!("PTY reader thread exiting");
            })?;

        Ok(Self {
            child,
            master_resizer: Arc::new(std::sync::Mutex::new(pair.master)),
            output_tx,
            writer: Arc::new(Mutex::new(writer)),
        })
    }

    /// Subscribe to PTY output. Returns a receiver that gets a copy of
    /// every chunk of bytes read from the PTY.
    /// Call this once for the pusher, once for the stdout forwarder, etc.
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Write bytes to the PTY (inject input into Claude Code's stdin).
    #[allow(dead_code)]
    pub async fn write_input(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize the PTY. Call when terminal dimensions change.
    pub fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let master = self.master_resizer.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// Inject bytes directly into the PTY output broadcast (as if the process wrote them).
    /// Used to prepend the startup banner so browser terminals see it in the session buffer.
    pub fn inject_output(&self, data: Vec<u8>) {
        let _ = self.output_tx.send(data);
    }

    /// Get a clonable resize handle for use from async tasks.
    pub fn resizer(&self) -> PtyResizer {
        self.master_resizer.clone()
    }

    /// Check if the child process has exited.
    pub fn try_wait(&mut self) -> anyhow::Result<Option<portable_pty::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Kill the child process.
    pub fn kill(&mut self) -> anyhow::Result<()> {
        self.child.kill()?;
        Ok(())
    }

    /// Get a clone of the writer handle (for sharing with the puller).
    pub fn writer_handle(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        self.writer.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_echo_and_read_output() {
        // Spawn `echo hello` in a PTY and verify we receive the output
        let mut pty = PtyManager::spawn_command("echo", &["hello"], 24, 80)
            .expect("Failed to spawn echo in PTY");

        let mut rx = pty.subscribe_output();

        // Collect output until the child exits (echo is instant)
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
        loop {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(chunk)) => output.extend_from_slice(&chunk),
                _ => break,
            }
        }

        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("hello"),
            "Expected output to contain 'hello', got: {:?}",
            output_str
        );

        // Child should have exited
        let status = pty.try_wait().expect("try_wait failed");
        assert!(status.is_some(), "echo should have exited");
    }

    #[tokio::test]
    async fn test_broadcast_multiple_subscribers() {
        // Verify that multiple subscribers all receive the same output
        let mut pty = PtyManager::spawn_command("echo", &["broadcast_test"], 24, 80)
            .expect("Failed to spawn");

        let mut rx1 = pty.subscribe_output();
        let mut rx2 = pty.subscribe_output();

        // Give the reader thread time to read
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let mut out1 = Vec::new();
        let mut out2 = Vec::new();

        // Drain both receivers
        while let Ok(chunk) = rx1.try_recv() {
            out1.extend_from_slice(&chunk);
        }
        while let Ok(chunk) = rx2.try_recv() {
            out2.extend_from_slice(&chunk);
        }

        // Both should have the same data
        assert_eq!(out1, out2, "Both subscribers should get identical output");
        let s = String::from_utf8_lossy(&out1);
        assert!(s.contains("broadcast_test"), "Output should contain 'broadcast_test'");
    }

    #[tokio::test]
    async fn test_write_input_to_cat() {
        // Spawn `cat` which echoes stdin to stdout, write to it, read back
        let mut pty = PtyManager::spawn_command("cat", &[], 24, 80)
            .expect("Failed to spawn cat");

        let mut rx = pty.subscribe_output();

        // Give cat a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Write input
        pty.write_input(b"test_input\n").await.expect("write_input failed");

        // Read output — cat should echo it back
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        loop {
            let remaining = deadline - tokio::time::Instant::now();
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(chunk)) => {
                    output.extend_from_slice(&chunk);
                    let s = String::from_utf8_lossy(&output);
                    if s.contains("test_input") {
                        break;
                    }
                }
                _ => break,
            }
        }

        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("test_input"),
            "Expected cat to echo 'test_input', got: {:?}",
            output_str
        );

        // Clean up
        let _ = pty.kill();
    }

    #[test]
    fn test_resize() {
        let pty = PtyManager::spawn_command("sleep", &["10"], 24, 80)
            .expect("Failed to spawn sleep");

        // Resize should not error
        pty.resize(40, 120).expect("resize failed");
        pty.resize(1, 1).expect("resize to minimum failed");
    }

    #[test]
    fn test_kill() {
        let mut pty = PtyManager::spawn_command("sleep", &["60"], 24, 80)
            .expect("Failed to spawn sleep");

        // Should not have exited yet
        let status = pty.try_wait().expect("try_wait failed");
        assert!(status.is_none(), "sleep should still be running");

        // Kill it
        pty.kill().expect("kill failed");

        // Give it a moment to register
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Should now be exited
        let status = pty.try_wait().expect("try_wait failed");
        assert!(status.is_some(), "sleep should have exited after kill");
    }

    #[test]
    fn test_writer_handle_is_shared() {
        let pty = PtyManager::spawn_command("cat", &[], 24, 80)
            .expect("Failed to spawn cat");

        let h1 = pty.writer_handle();
        let h2 = pty.writer_handle();

        // Both handles should point to the same Arc
        assert!(Arc::ptr_eq(&h1, &h2), "writer_handle should return clones of the same Arc");
    }
}
