use std::path::PathBuf;

use anyhow::{bail, Context};
use chrono::{Duration, Utc};

use crate::config::Config;

// ── spawn ───────────────────────────────────────────────────────────

pub async fn spawn(name: &str, expires_in: Option<&str>) -> anyhow::Result<()> {
    let cfg = Config::load().context("no agent config found — run `b3 setup` first")?;

    let expires_at = expires_in.map(parse_duration).transpose()?;

    let body = serde_json::json!({
        "name": name,
        "expires_at": expires_at.map(|d| (Utc::now() + d).to_rfc3339()),
    });

    let client = reqwest::Client::new();
    let url = format!("{}/api/agents/{}/children", cfg.api_url, cfg.agent_id);
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .json(&body)
        .send()
        .await
        .context("request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("spawn failed ({status}): {text}");
    }

    let data: serde_json::Value = resp.json().await.context("invalid response")?;

    // Write child config under <cwd>/<name>/.b3/config.json
    let child_dir = std::env::current_dir()?.join(name);
    let b3_dir = child_dir.join(".b3");
    std::fs::create_dir_all(&b3_dir)
        .with_context(|| format!("failed to create {}", b3_dir.display()))?;

    let child_config = serde_json::json!({
        "agent_id": data["id"],
        "agent_email": format!("{}@{}", name, extract_domain(&cfg.api_url)),
        "api_key": data["api_key"],
        "api_url": cfg.api_url,
        "wg_address": data["wg_address"].as_str().unwrap_or(""),
        "relay_endpoint": cfg.relay_endpoint,
        "relay_public_key": cfg.relay_public_key,
        "push_interval_ms": cfg.push_interval_ms,
        "web_url": data["dashboard_url"],
        "b3_version": env!("CARGO_PKG_VERSION"),
        "servers": cfg.servers,
        "parent_id": data["parent_id"],
    });

    let config_path = b3_dir.join("config.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(&child_config)?)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    // Write WG private key to <child>/.b3/wg/private.key — same layout as setup.rs
    // so the daemon's crypto::load_wg_private_key() finds it.
    if let Some(wg_priv) = data["wg_private_key"].as_str() {
        let wg_dir = b3_dir.join("wg");
        std::fs::create_dir_all(&wg_dir)
            .with_context(|| format!("failed to create {}", wg_dir.display()))?;
        let wg_path = wg_dir.join("private.key");
        std::fs::write(&wg_path, wg_priv)
            .with_context(|| format!("failed to write {}", wg_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wg_dir, std::fs::Permissions::from_mode(0o700)).ok();
            std::fs::set_permissions(&wg_path, std::fs::Permissions::from_mode(0o600)).ok();
        }
    }

    let dashboard_url = data["dashboard_url"].as_str().unwrap_or("");
    let address = data["address"].as_str().unwrap_or(name);
    println!("✓ Child agent spawned: {address}");
    println!("  Folder: {}", child_dir.display());
    println!("  Dashboard: {dashboard_url}");
    println!("  To start: cd {} && b3 start", child_dir.display());

    Ok(())
}

// ── list ────────────────────────────────────────────────────────────

pub async fn list(show_all: bool) -> anyhow::Result<()> {
    let cfg = Config::load().context("no agent config found")?;

    let client = reqwest::Client::new();
    let mut url = format!("{}/api/agents/{}/children", cfg.api_url, cfg.agent_id);
    if show_all {
        url.push_str("?all=1");
    }
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .send()
        .await
        .context("request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("list failed ({status}): {text}");
    }

    let data: serde_json::Value = resp.json().await.context("invalid response")?;
    let children = data["children"].as_array().cloned().unwrap_or_default();

    if children.is_empty() {
        println!("No child agents.");
        return Ok(());
    }

    println!("{:<30} {:<12} {:<26} {:<26}", "ADDRESS", "STATUS", "CREATED", "EXPIRES");
    println!("{}", "-".repeat(98));
    for child in &children {
        let address = child["address"].as_str().unwrap_or("?");
        let id = child["id"].as_str().unwrap_or("?");
        let status = child["status"].as_str().unwrap_or("?");
        let created = child["created_at"].as_str().unwrap_or("?");
        let expires = child["expires_at"].as_str().unwrap_or("never");
        if show_all && status == "deleted" {
            println!("{:<30} {:<12} {:<26} {:<26}  (id: {id})", address, status, created, expires);
        } else {
            println!("{:<30} {:<12} {:<26} {:<26}", address, status, created, expires);
        }
    }

    Ok(())
}

// ── kill ────────────────────────────────────────────────────────────

pub async fn kill(name_or_id: &str) -> anyhow::Result<()> {
    let cfg = Config::load().context("no agent config found")?;

    // If it looks like an id (hc-*), use it directly. Otherwise resolve by name.
    let child_id = if name_or_id.starts_with("hc-") {
        name_or_id.to_string()
    } else {
        resolve_child_id(&cfg, name_or_id).await?
    };

    let client = reqwest::Client::new();
    let url = format!("{}/api/agents/{}/kill", cfg.api_url, child_id);
    let resp = client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .send()
        .await
        .context("request failed")?;

    match resp.status().as_u16() {
        204 => println!("✓ Child agent {name_or_id} killed."),
        404 => bail!("child agent not found: {name_or_id}"),
        403 => bail!("permission denied — not your child agent"),
        s => {
            let text = resp.text().await.unwrap_or_default();
            bail!("kill failed ({s}): {text}");
        }
    }

    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────

async fn resolve_child_id(cfg: &Config, name: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/agents/{}/children", cfg.api_url, cfg.agent_id);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .send()
        .await
        .context("request failed")?;

    if !resp.status().is_success() {
        bail!("could not list children to resolve name '{name}'");
    }

    let data: serde_json::Value = resp.json().await?;
    let children = data["children"].as_array().cloned().unwrap_or_default();

    for child in &children {
        if child["name"].as_str() == Some(name) {
            if let Some(id) = child["id"].as_str() {
                return Ok(id.to_string());
            }
        }
    }

    bail!("no active child agent named '{name}'")
}

fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let (num_str, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num_str.parse().context("invalid duration number")?;
    match unit {
        "s" => Ok(Duration::seconds(n)),
        "m" => Ok(Duration::minutes(n)),
        "h" => Ok(Duration::hours(n)),
        "d" => Ok(Duration::days(n)),
        _ => bail!("unknown duration unit '{unit}' — use s, m, h, or d"),
    }
}

// ── restore ─────────────────────────────────────────────────────────

pub async fn restore(name_or_id: &str) -> anyhow::Result<()> {
    let cfg = Config::load().context("no agent config found")?;

    let child_id = if name_or_id.starts_with("hc-") {
        name_or_id.to_string()
    } else {
        resolve_deleted_child_id(&cfg, name_or_id).await?
    };

    let client = reqwest::Client::new();
    let url = format!("{}/api/agents/{}/restore", cfg.api_url, child_id);
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .send()
        .await
        .context("request failed")?;

    match resp.status().as_u16() {
        200 => {
            let data: serde_json::Value = resp.json().await.context("invalid response")?;
            let name = data["name"].as_str().unwrap_or(name_or_id);
            let new_key = data["api_key"].as_str().unwrap_or("");
            println!("✓ Child agent {name} restored.");
            // Update the child's config.json with the new api_key if the folder exists.
            if !new_key.is_empty() {
                let child_dir = std::env::current_dir()?.join(name);
                let config_path = child_dir.join(".b3").join("config.json");
                if config_path.exists() {
                    if let Ok(raw) = std::fs::read_to_string(&config_path) {
                        if let Ok(mut cfg) = serde_json::from_str::<serde_json::Value>(&raw) {
                            cfg["api_key"] = serde_json::Value::String(new_key.to_string());
                            if let Ok(updated) = serde_json::to_string_pretty(&cfg) {
                                let _ = std::fs::write(&config_path, updated);
                                println!("  Config updated: {}", config_path.display());
                            }
                        }
                    }
                }
            }
        }
        404 => anyhow::bail!("child agent not found, not yours, or not deleted: {name_or_id}"),
        403 => anyhow::bail!("permission denied — not your child agent"),
        s => {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("restore failed ({s}): {text}");
        }
    }

    Ok(())
}

async fn resolve_deleted_child_id(cfg: &Config, name: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/agents/{}/children", cfg.api_url, cfg.agent_id);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .send()
        .await
        .context("request failed")?;

    if !resp.status().is_success() {
        anyhow::bail!("could not list children to resolve name '{name}'");
    }

    // list_children excludes status='deleted', so query for deleted separately
    // by scanning all children including deleted via a direct id lookup.
    // For simplicity: try the name as-is and let the server's 404 handle it.
    // The server checks name via the restore endpoint — but we need an id.
    // Best effort: list active children first, then rely on server 404 for deleted-only lookup.
    let data: serde_json::Value = resp.json().await?;
    let children = data["children"].as_array().cloned().unwrap_or_default();

    for child in &children {
        if child["name"].as_str() == Some(name) {
            if let Some(id) = child["id"].as_str() {
                return Ok(id.to_string());
            }
        }
    }

    // Not found in active list — the server restore endpoint also accepts names
    // by resolving them, but our API only takes IDs. Ask user to use the ID.
    anyhow::bail!(
        "no child agent named '{name}' found in active list — if it's deleted, use its id (hc-XXXXXX)"
    )
}

fn extract_domain(api_url: &str) -> String {
    api_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("babel3.com")
        .to_string()
}
