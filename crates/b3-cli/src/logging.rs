//! Log rotation and agent.log management.
//!
//! Writes to ~/.b3/agent.log
//! Rotation: 10MB max, 3 files kept
//! All errors logged + printed to stderr if interactive
