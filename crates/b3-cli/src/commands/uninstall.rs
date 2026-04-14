//! b3 uninstall — Remove Babel3: stop services, deregister agent, clean up config.
//!
//! File restoration strategy:
//!   - If a .b3-backup exists → offer to restore from backup (exact original)
//!   - Always available: surgical removal (just remove b3 entries, preserve everything else)
//!   - User chooses which approach for each file

use std::io::{self, Write};

use crate::config::Config;
use crate::daemon::server;

pub async fn run() -> anyhow::Result<()> {
    println!();
    println!("  Babel3 Uninstall");
    println!("  =================");
    println!();

    if !Config::exists() {
        println!("  Babel3 is not installed (no config found).");
        println!("  Nothing to remove.");
        return Ok(());
    }

    let config = Config::load()?;

    println!("  This will:");
    println!("    1. Stop the running daemon (if any)");
    println!("    2. Deregister agent {} from the server", config.agent_email);
    println!("    3. Delete all config files (~/.b3/)");
    println!("    4. Clean up .mcp.json and CLAUDE.md");
    println!();
    print!("  Are you sure? [y/N]: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        println!("  Uninstall cancelled.");
        return Ok(());
    }
    println!();

    // 1. Stop daemon if running
    if server::is_running() {
        println!("  Stopping daemon...");
        match crate::commands::stop::run(true).await {
            Ok(_) => println!("  ✓ Daemon stopped"),
            Err(e) => println!("  Note: Could not stop daemon cleanly: {e}"),
        }
    } else {
        println!("  ✓ Daemon not running");
    }

    // 2. Deregister from server
    println!("  Deregistering agent...");
    let client = crate::http::build_client(std::time::Duration::from_secs(10))?;

    let url = format!("{}/api/agents/{}", config.api_url, config.agent_id);
    match client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            println!("  ✓ Agent deregistered from server");
        }
        Ok(resp) => {
            println!(
                "  Note: Server returned {} (agent may already be removed)",
                resp.status()
            );
        }
        Err(e) => {
            println!("  Note: Could not reach server: {e}");
            println!("        (config will still be removed locally)");
        }
    }

    // 3. Remove config directory
    let config_dir = Config::config_dir();
    if config_dir.exists() {
        println!("  Removing {}...", config_dir.display());
        std::fs::remove_dir_all(&config_dir)?;
        println!("  ✓ Config removed");
    }

    // 4. Clean up .mcp.json
    let cwd = std::env::current_dir()?;
    let mcp_path = cwd.join(".mcp.json");
    let mcp_backup = cwd.join(".mcp.json.b3-backup");

    if mcp_path.exists() {
        if mcp_backup.exists() {
            // Offer choice: restore backup or surgical removal
            println!();
            println!("  .mcp.json has a pre-Babel3 backup.");
            print!("  [R]estore backup or [S]urgically remove b3 entry? [R/s]: ");
            io::stdout().flush()?;
            let mut choice = String::new();
            io::stdin().read_line(&mut choice)?;
            if choice.trim().eq_ignore_ascii_case("s") {
                // Surgical: remove just b3-voice, keep backup around
                remove_mcp_entry(&mcp_path)?;
                println!("  ✓ Removed b3-voice from .mcp.json (backup kept at .mcp.json.b3-backup)");
            } else {
                // Restore: replace with exact original
                std::fs::copy(&mcp_backup, &mcp_path)?;
                std::fs::remove_file(&mcp_backup)?;
                println!("  ✓ Restored .mcp.json from pre-Babel3 backup");
            }
        } else {
            // No backup — surgical removal only option
            if remove_mcp_entry(&mcp_path)? {
                println!("  ✓ Removed b3-voice from .mcp.json");
            }
        }
    }

    // 5. Clean up CLAUDE.md
    let claude_md = cwd.join("CLAUDE.md");
    let claude_backup = cwd.join("CLAUDE.md.b3-backup");

    if claude_md.exists() {
        let content = std::fs::read_to_string(&claude_md).unwrap_or_default();
        let has_b3_section = content.contains("## Babel3 Voice");

        if has_b3_section {
            if claude_backup.exists() {
                println!();
                println!("  CLAUDE.md has a pre-Babel3 backup.");
                print!("  [R]estore backup or [S]urgically remove Babel3 section? [R/s]: ");
                io::stdout().flush()?;
                let mut choice = String::new();
                io::stdin().read_line(&mut choice)?;
                if choice.trim().eq_ignore_ascii_case("s") {
                    remove_claude_section(&claude_md)?;
                    println!("  ✓ Removed Babel3 Voice section (backup kept at CLAUDE.md.b3-backup)");
                } else {
                    std::fs::copy(&claude_backup, &claude_md)?;
                    std::fs::remove_file(&claude_backup)?;
                    println!("  ✓ Restored CLAUDE.md from pre-Babel3 backup");
                }
            } else {
                remove_claude_section(&claude_md)?;
                println!("  ✓ Removed Babel3 Voice section from CLAUDE.md");
            }
        } else if claude_backup.exists() {
            // Section already gone but backup lingers — clean up
            std::fs::remove_file(&claude_backup)?;
        }
    }

    println!();
    println!("  ══════════════════════════════════════════");
    println!("  ✓ Babel3 uninstalled.");
    println!();
    println!("  The b3 binary is still at its install location.");
    println!("  To remove it: rm $(which b3)");
    println!("  ══════════════════════════════════════════");
    println!();

    Ok(())
}

/// Surgically remove just the b3-voice entry from .mcp.json.
/// Returns true if an entry was removed.
fn remove_mcp_entry(mcp_path: &std::path::Path) -> anyhow::Result<bool> {
    let data = std::fs::read_to_string(mcp_path)?;
    let mut config: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return Ok(false), // Can't parse — leave it alone
    };

    if let Some(servers) = config.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
        if servers.remove("b3").is_some() {
            let json_str = serde_json::to_string_pretty(&config)?;
            std::fs::write(mcp_path, json_str)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Surgically remove the "## Babel3 Voice" section from CLAUDE.md.
fn remove_claude_section(claude_md: &std::path::Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(claude_md)?;
    if let Some(idx) = content.find("\n\n## Babel3 Voice\n") {
        let cleaned = content[..idx].to_string();
        std::fs::write(claude_md, cleaned)?;
    }
    Ok(())
}
