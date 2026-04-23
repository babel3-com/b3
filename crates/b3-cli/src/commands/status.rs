//! b3 status — Show agent identity, daemon state, connectivity.

use crate::config::Config;
use crate::daemon::server;

pub async fn run() -> anyhow::Result<()> {
    // Check if set up
    if !Config::exists() {
        println!("Status: NOT SET UP");
        println!("Run `b3 setup` to register this agent.");
        return Ok(());
    }

    let config = Config::load()?;

    // Identity
    println!("Agent Identity");
    println!("  Email:     {}", config.agent_email);
    println!("  ID:        {}", config.agent_id);
    println!("  Dashboard: {}", config.web_url);
    println!();

    // Daemon state
    println!("Daemon");
    if server::is_running() {
        println!("  Status:    running");
        println!("  PID file:  {}", server::pid_path().display());
        if let Ok(contents) = std::fs::read_to_string(server::pid_path()) {
            println!("  PID:       {}", contents.trim());
        }
        // Development mode indicator
        if let Some(dir) = crate::daemon::fork_state::browser_dir() {
            println!("  Browser:   development ({})", dir.display());
        } else {
            println!("  Browser:   official");
        }
    } else {
        println!("  Status:    stopped");
    }
    println!();

    // Connectivity
    println!("Connectivity");
    let client = crate::http::build_client(std::time::Duration::from_secs(5))?;

    // Public API (HTTPS)
    let public_url = format!("{}/health", config.api_url);
    match client.get(&public_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            println!("  Public API:  ✓ reachable ({})", config.api_url);
        }
        Ok(resp) => {
            println!("  Public API:  ✗ status {} ({})", resp.status(), config.api_url);
        }
        Err(e) => {
            println!("  Public API:  ✗ unreachable ({})", e);
        }
    }

    Ok(())
}
