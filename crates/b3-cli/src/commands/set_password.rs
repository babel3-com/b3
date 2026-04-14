//! b3 set-password — Set or change the daemon password.
//!
//! Interactive mode (default): prompts for password + confirmation.
//! Non-interactive mode (--password): for scripting / agent-manager.

use crate::config::Config;
use std::io::{self, Write};

pub async fn run(password: Option<String>) -> anyhow::Result<()> {
    if !Config::exists() {
        anyhow::bail!("Babel3 is not set up. Run `b3 setup` first.");
    }

    let password = if let Some(pw) = password {
        // Non-interactive mode
        pw
    } else {
        // Interactive mode
        eprint!("New daemon password: ");
        io::stderr().flush()?;
        let pw = read_password_line()?;

        eprint!("Confirm password: ");
        io::stderr().flush()?;
        let confirm = read_password_line()?;

        if pw != confirm {
            anyhow::bail!("Passwords don't match.");
        }
        pw
    };

    if password.len() < 4 {
        anyhow::bail!("Password must be at least 4 characters.");
    }

    let hash = Config::hash_daemon_password(&password);
    Config::save_daemon_password_hash(&hash)?;

    println!("Daemon password updated.");
    println!("Restart the daemon for the change to take effect.");
    Ok(())
}

/// Read a line from stdin (no echo suppression — keeping it simple).
fn read_password_line() -> anyhow::Result<String> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
