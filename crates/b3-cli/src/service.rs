//! systemd/launchd service install and management.
//!
//! Linux: generates /etc/systemd/system/b3.service (Restart=on-failure)
//! macOS: generates ~/Library/LaunchAgents/ai.babel3.plist (KeepAlive=true)
//! Auto-starts on boot, PTY session survives detach/reattach.
