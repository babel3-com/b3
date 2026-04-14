//! b3 setup — First-time install: register agent with the Babel3 server.
//!
//! Flow:
//!   1. Check if already set up
//!   2. Prompt for setup token (from {domain}/dashboard)
//!   3. POST /api/agents/register → receive credentials
//!   4. Store credentials in ~/.b3/config.json
//!   5. Store WG private key in ~/.b3/wg/private.key
//!   6. Register Voice MCP in .mcp.json (current directory)
//!   7. Print success with dashboard URL

use std::io::{self, Write};
use std::path::PathBuf;

use b3_common::{public_url, public_domain, RegisterRequest, RegisterResponse};
use serde_json::{json, Value};

use crate::config::Config;
use crate::daemon::server;

pub async fn run(cli_token: Option<String>, cli_name: Option<String>) -> anyhow::Result<()> {
    let non_interactive = cli_token.is_some();

    if !non_interactive {
        println!();
        println!("  Babel3 Setup");
        println!("  =============");
        println!();
    }

    // 1. Check if already set up (skip confirmation in non-interactive mode)
    if Config::exists() && !non_interactive {
        print!("  Babel3 is already configured. Overwrite? [y/N]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("  Setup cancelled.");
            return Ok(());
        }
        println!();
    }

    // 2. API URL (default production, overridable for testing)
    let api_url = std::env::var("B3_API_URL")
        .unwrap_or_else(|_| public_url());

    // 3. Get setup token
    let token = if let Some(t) = cli_token {
        // Non-interactive: token provided via --token flag
        t
    } else {
        // Interactive: open browser and prompt
        let account_url = format!("{api_url}/account#agents");
        println!("  Opening your browser to get a setup token...");
        println!();
        println!("  If the browser doesn't open, visit:");
        println!("  {account_url}");
        println!();

        let _ = open_browser(&account_url);

        println!("  1. Log in (or sign up) at {}", public_domain());
        println!("  2. Go to the Agents tab");
        println!("  3. Click \"Generate Setup Token\"");
        println!("  4. Paste the token below");
        println!();
        print!("  Token: ");
        io::stdout().flush()?;
        let mut token = String::new();
        io::stdin().read_line(&mut token)?;
        let token = token.trim().to_string();

        if token.is_empty() {
            anyhow::bail!("No token provided. Visit {account_url} to generate one.");
        }
        token
    };

    // 4. Get agent name
    let default_name = gethostname::gethostname()
        .to_string_lossy()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>();

    let requested_name = if let Some(name) = cli_name {
        // Non-interactive: name provided via --name flag
        name
    } else if non_interactive {
        // Non-interactive without --name: use hostname default
        default_name
    } else {
        // Interactive: prompt
        println!();
        print!("  Agent name [{}]: ", default_name);
        io::stdout().flush()?;
        let mut name_input = String::new();
        io::stdin().read_line(&mut name_input)?;
        let name_input = name_input.trim().to_string();
        if name_input.is_empty() { default_name } else { name_input }
    };

    // 5. Collect system info
    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .to_string();

    let platform = format!(
        "{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    println!("  Registering agent as \"{}\"...", requested_name);

    // 6. POST /api/agents/register
    let client = reqwest::Client::new();
    let req_body = RegisterRequest {
        token,
        hostname,
        platform,
        requested_name: Some(requested_name),
    };

    let resp = client
        .post(format!("{api_url}/api/agents/register"))
        .json(&req_body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Registration failed ({status}): {body}");
    }

    let reg: RegisterResponse = resp.json().await?;

    // 6. Save config (api_url is public, used only for this initial registration;
    //    all runtime comms go through the WireGuard mesh)
    let config = Config {
        agent_id: reg.agent_id.clone(),
        agent_email: reg.agent_email.clone(),
        api_key: reg.api_key.clone(),
        api_url: api_url.clone(),
        wg_address: reg.wg_address.clone(),
        relay_endpoint: reg.relay_endpoint.clone(),
        relay_public_key: reg.relay_public_key.clone(),
        push_interval_ms: 100,
        web_url: reg.web_url.clone(),
        b3_version: env!("CARGO_PKG_VERSION").to_string(),
        servers: reg.servers.clone(),
    };
    config.save()?;

    // 7. Save WG private key
    let wg_dir = Config::config_dir().join("wg");
    std::fs::create_dir_all(&wg_dir)?;
    let key_path = wg_dir.join("private.key");
    std::fs::write(&key_path, &reg.wg_private_key)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wg_dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    // 7b. Stop stale daemon if running (it has old credentials)
    if server::is_running() {
        println!("  Stopping running daemon (stale credentials)...");
        match crate::commands::stop::run(true).await {
            Ok(_) => {
                println!("  Old daemon stopped. New credentials will be used on next start.");
            }
            Err(e) => {
                eprintln!("  Warning: Could not stop running daemon: {e}");
                eprintln!("  Run `b3 stop && b3 start` manually.");
            }
        }
    }

    // 7c. Set daemon password (required for browser dashboard access)
    if !non_interactive {
        println!();
        println!("  Set a password for browser dashboard access:");
        print!("  Password: ");
        io::stdout().flush()?;
        let mut pw = String::new();
        io::stdin().read_line(&mut pw)?;
        let pw = pw.trim().to_string();
        if pw.len() >= 4 {
            let hash = Config::hash_daemon_password(&pw);
            Config::save_daemon_password_hash(&hash)?;
            println!("  ✓ Dashboard password set");
        } else if !pw.is_empty() {
            println!("  ⚠ Password too short (min 4 chars) — skipped. Set later with: b3 set-password");
        } else {
            println!("  ⚠ No password set — set later with: b3 set-password");
        }
    }

    // 8. Register Voice MCP with Claude Code + Codex
    let b3_bin = find_b3_binary();
    let mcp_registered = register_voice_mcp(&b3_bin);
    let codex_registered = register_codex_mcp(&b3_bin);

    // 8b. Append voice instructions to CLAUDE.md and AGENTS.md
    let claude_md_result = append_voice_instructions(&reg.agent_email, &reg.web_url);
    let agents_md_result = append_agents_md_instructions(&reg.agent_email, &reg.web_url);

    // 9. Success banner
    println!();
    println!("  ══════════════════════════════════════════");
    println!("  ✓ Agent registered!");
    println!();
    println!("    Identity:  {}", reg.agent_email);
    println!("    Agent ID:  {}", reg.agent_id);
    println!("    Mesh IP:   {}", reg.wg_address);
    println!("    Dashboard: {}", reg.web_url);
    println!();
    println!("  Config saved to ~/.b3/config.json");

    match mcp_registered {
        Ok(path) => {
            println!("  Voice MCP registered in {}", path.display());
        }
        Err(e) => {
            eprintln!("  Note: Could not auto-register Voice MCP: {e}");
            println!();
            println!("  To register manually, add to your .mcp.json:");
            println!("    {{");
            println!("      \"mcpServers\": {{");
            println!("        \"b3\": {{");
            println!("          \"command\": \"{}\",", b3_bin.display());
            println!("          \"args\": [\"mcp\", \"voice\"]");
            println!("        }}");
            println!("      }}");
            println!("    }}");
        }
    }

    if let Ok(path) = claude_md_result {
        println!("  Voice instructions added to {}", path.display());
    }
    if let Ok(path) = agents_md_result {
        println!("  Voice instructions added to {}", path.display());
    }
    match codex_registered {
        Ok(path) => println!("  Codex MCP registered in {}", path.display()),
        Err(_) => {} // Codex not installed — silent, not an error
    }

    println!("  ══════════════════════════════════════════");
    println!();

    Ok(())
}

/// Find the b3 binary path. Prefers the installed location,
/// falls back to current executable path.
pub fn find_b3_binary() -> PathBuf {
    // Check common install locations first
    let bin_name = if cfg!(windows) { "b3.exe" } else { "b3" };
    let mut candidates: Vec<Option<PathBuf>> = vec![
        dirs::home_dir().map(|h| h.join(".local/bin").join(bin_name)),
    ];
    #[cfg(unix)]
    candidates.push(Some(PathBuf::from("/usr/local/bin/b3")));
    #[cfg(windows)]
    candidates.push(
        std::env::var("LOCALAPPDATA").ok().map(|d| PathBuf::from(d).join("Babel3\\bin\\b3.exe")),
    );
    for candidate in candidates.iter().flatten() {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    // Fall back to current executable
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("b3"))
}

/// Append voice instructions to CLAUDE.md in the current directory.
/// NON-DESTRUCTIVE: only appends, never overwrites existing content.
/// Creates a CLAUDE.md.b3-backup before modifying, so `b3 uninstall` can restore.
/// Returns the path on success.
fn append_voice_instructions(agent_name: &str, web_url: &str) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let claude_md_path = cwd.join("CLAUDE.md");

    // Check if voice instructions already exist
    if claude_md_path.exists() {
        let existing = std::fs::read_to_string(&claude_md_path)?;
        if existing.contains("## Babel3 Voice") {
            return Ok(claude_md_path);
        }
        // Backup existing CLAUDE.md before appending (first time only)
        let backup_path = cwd.join("CLAUDE.md.b3-backup");
        if !backup_path.exists() {
            let _ = std::fs::copy(&claude_md_path, &backup_path);
        }
    }

    let current_version = env!("CARGO_PKG_VERSION");
    let version_tag = format!("<!-- b3 v{current_version} -->");
    let instructions = format!(
        "\n\n## Babel3 Voice\n\n\
         This project has Babel3 voice integration. The user can talk to you from their phone\n\
         at {web_url} and you can speak responses aloud.\n\n\
         **IMPORTANT: You have voice tools via the `b3` MCP. They ARE available and working.\n\
         Call them directly — do NOT check resources or capabilities first. Just call the tool.**\n\n\
         **Voice tools (call these directly):**\n\n\
         - `say(text, emotion, replying_to)` — Speak text aloud to the user's browser. All three parameters are REQUIRED.\n\
         - `voice_status()` — Check voice pipeline health.\n\
         - `voice_health()` — Deep health check (curls endpoints directly).\n\
         - `voice_logs(lines?)` — Get recent daemon logs.\n\
         - `voice_delivery_status(msg_id)` — Check message delivery.\n\
         - `voice_share_info(html)` — Push HTML content to connected browsers.\n\
         - `voice_show_image(path)` — Push an image to connected browsers.\n\
         - `voice_enroll_speakers(embeddings_dir)` — Upload speaker embeddings for diarization.\n\n\
         **When you receive voice input** (via <channel> tags or lines starting with `[M`), ALWAYS respond with `say()`.\n\
         The user is on their phone — text responses go to a terminal they're not watching.\n\
         Voice goes to their ears. Audio in = audio out.\n\n\
         **ALWAYS include all three parameters** when calling `say()`. The `replying_to` echoes back what the\n\
         user said (or a summary of context). The `emotion` drives the LED chromatophore animation on the\n\
         dashboard — it maps to colors and patterns via embeddings.\n\
         Use a short, natural English phrase describing the emotional tone (e.g., \"warm joy\", \"thoughtful\",\n\
         \"excited curiosity\", \"gentle reassurance\", \"playful mischief\"). Be specific and varied — avoid\n\
         repeating the same emotion. If unsure, use \"neutral calm\".\n\n\
         **Example:** User says \"Testing 1 2 3\" → you call `say(text=\"I can hear you! Your microphone is working great.\", emotion=\"cheerful enthusiasm\", replying_to=\"Testing 1 2 3\")`\n\n\
         Agent: {agent_name}\n\
         {version_tag}\n",
    );

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&claude_md_path)?;
    std::io::Write::write_all(&mut file, instructions.as_bytes())?;

    Ok(claude_md_path)
}

/// Open a URL in the default browser. Best-effort — never fails, never spams stderr.
/// In headless/SSH/Docker environments, silently does nothing.
fn open_browser(url: &str) -> anyhow::Result<()> {
    use std::process::Stdio;

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        // Try xdg-open first, then wslview for WSL — suppress all output
        let opened = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok();
        if !opened {
            let _ = std::process::Command::new("wslview")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    Ok(())
}

/// Register the Voice MCP in .mcp.json in the current working directory.
/// NON-DESTRUCTIVE: reads existing .mcp.json and only adds the b3 entry.
/// Never overwrites or destroys existing MCP configurations.
/// Creates a .mcp.json.b3-backup before modifying, so `b3 uninstall` can restore.
/// Returns the path to the .mcp.json file on success.
pub fn register_voice_mcp(b3_bin: &PathBuf) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let mcp_path = cwd.join(".mcp.json");

    let mut config: Value = if mcp_path.exists() {
        let data = std::fs::read_to_string(&mcp_path)?;
        match serde_json::from_str(&data) {
            Ok(parsed) => {
                // Backup the original before we touch it
                let backup_path = cwd.join(".mcp.json.b3-backup");
                if !backup_path.exists() {
                    // Only create backup the first time — don't overwrite previous backup
                    let _ = std::fs::copy(&mcp_path, &backup_path);
                }
                parsed
            }
            Err(e) => {
                // Existing file is not valid JSON — DO NOT overwrite it.
                // The user may have comments, trailing commas, or a format we don't understand.
                eprintln!("  ⚠ Warning: .mcp.json exists but could not be parsed: {e}");
                eprintln!("    Your existing config has been left untouched.");
                anyhow::bail!(
                    "Cannot safely merge into .mcp.json (parse error: {e}). \
                     Please add b3 MCP entry manually."
                );
            }
        }
    } else {
        json!({ "mcpServers": {} })
    };

    // Ensure mcpServers object exists
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }

    // Merge into existing b3 entry to preserve user-added fields (e.g. env).
    // Version stamp lets us manage upgrades/downgrades.
    let existing = config["mcpServers"]
        .get("b3")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut entry = if existing.is_object() { existing } else { json!({}) };
    entry["command"] = json!(b3_bin.to_string_lossy());
    entry["args"] = json!(["mcp", "voice"]);
    entry["_b3_version"] = json!(env!("CARGO_PKG_VERSION"));
    config["mcpServers"]["b3"] = entry;

    let json_str = serde_json::to_string_pretty(&config)?;
    std::fs::write(&mcp_path, &json_str)?;

    Ok(mcp_path)
}

/// Register the Voice MCP with OpenAI Codex CLI (~/.codex/config.toml).
/// NON-DESTRUCTIVE: reads existing config.toml and only adds/updates the b3 entry.
/// Returns the path on success, or Err if Codex is not installed.
pub fn register_codex_mcp(b3_bin: &PathBuf) -> anyhow::Result<PathBuf> {
    let codex_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .join(".codex");

    if !codex_dir.exists() {
        anyhow::bail!("Codex not installed (~/.codex not found)");
    }

    let config_path = codex_dir.join("config.toml");
    let mut content = if config_path.exists() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    // Check if b3 MCP is already registered
    if content.contains("[mcp_servers.b3]") {
        // Update the command path in case the binary moved
        let new_section = format!(
            "[mcp_servers.b3]\ncommand = \"{}\"\nargs = [\"mcp\", \"voice\"]\n",
            b3_bin.display()
        );
        // Replace the existing section
        if let Some(start) = content.find("[mcp_servers.b3]") {
            // Find the end of this section (next [section] or EOF)
            let rest = &content[start..];
            let end = rest[1..].find("\n[")
                .map(|i| start + 1 + i + 1)
                .unwrap_or(content.len());
            content.replace_range(start..end, &new_section);
        }
    } else {
        // Append new section
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!(
            "\n[mcp_servers.b3]\ncommand = \"{}\"\nargs = [\"mcp\", \"voice\"]\n",
            b3_bin.display()
        ));
    }

    std::fs::write(&config_path, &content)?;
    Ok(config_path)
}

/// Append voice instructions to AGENTS.md in the current directory (for Codex).
/// Same pattern as append_voice_instructions but targets AGENTS.md.
fn append_agents_md_instructions(agent_name: &str, web_url: &str) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let agents_md_path = cwd.join("AGENTS.md");

    // Check if voice instructions already exist
    if agents_md_path.exists() {
        let existing = std::fs::read_to_string(&agents_md_path)?;
        if existing.contains("## Babel3 Voice") {
            return Ok(agents_md_path);
        }
    }

    let instructions = format!(
        "\n\n## Babel3 Voice\n\n\
         This project has Babel3 voice integration. The user can talk to you from their phone\n\
         at {web_url} and you can speak responses aloud.\n\n\
         **Voice tools (via b3 MCP):**\n\n\
         - `voice_say(text, emotion)` — Speak text aloud. The `emotion` parameter is REQUIRED.\n\
         - `voice_status()` — Check voice pipeline health.\n\
         - `hive_send(target, message)` — Send a message to another agent.\n\
         - `hive_status()` — List sibling agents.\n\n\
         **When you receive voice input** (lines starting with `[M`), ALWAYS respond with `voice_say()`.\n\
         Audio in = audio out.\n\n\
         Agent: {agent_name}\n",
    );

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&agents_md_path)?;
    std::io::Write::write_all(&mut file, instructions.as_bytes())?;

    Ok(agents_md_path)
}
