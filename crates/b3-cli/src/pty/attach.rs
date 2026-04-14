//! Terminal attach/detach logic.
//!
//! Connects user's terminal to running PTY via Unix socket.
//! - Puts terminal in raw mode
//! - Bidirectional copy: user stdin → PTY, PTY → user stdout
//! - Detach on Ctrl+B d (configurable prefix key)
//! - Restores terminal mode on detach
