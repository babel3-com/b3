//! b3 hive — Agent-to-agent messaging.
//!
//! `b3 hive send <agent> <message>` — send a direct message.
//! `b3 hive list` — list agents belonging to the same user.
//! `b3 hive rooms` — list conversation rooms.
//! `b3 hive room <room-id>` — show room messages.
//! `b3 hive converse <room-id> <message>` — send to a room.
//!
//! Messages are delivered via SSE → PTY injection:
//!   Direct:  [HIVE from={sender}] {message}\n
//!   Room:    [HIVE-CONVERSATION] [{topic}] from={sender}: {message}\n

use crate::config::Config;
use crate::http;
use b3_common::HiveSendRequest;

fn build_client() -> anyhow::Result<(Config, reqwest::Client)> {
    if !Config::exists() {
        anyhow::bail!("Not set up. Run `b3 setup` first.");
    }
    let config = Config::load()?;
    let client = http::build_client(std::time::Duration::from_secs(10))?;
    Ok((config, client))
}

/// Send a hive message to another agent (same user).
pub async fn send(target_agent: &str, message: &str) -> anyhow::Result<()> {
    let (config, client) = build_client()?;

    let url = format!("{}/api/hive/send", config.api_url);
    let req = HiveSendRequest {
        target: target_agent.to_string(),
        message: message.to_string(),
        ephemeral: false,
        deliver_at: None,
        unlock_key: None,
    };

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .json(&req)
        .send()
        .await?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await?;
        let to = body.get("to").and_then(|v| v.as_str()).unwrap_or(target_agent);
        println!("Message sent to {to}");
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to send message: HTTP {status} — {body}");
    }

    Ok(())
}

/// List agents belonging to the same user.
pub async fn list() -> anyhow::Result<()> {
    let (config, client) = build_client()?;

    let url = format!("{}/api/hive/agents", config.api_url);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to list agents: HTTP {status} — {body}");
    }

    let body: serde_json::Value = resp.json().await?;
    let agents = body.get("agents").and_then(|v| v.as_array());

    match agents {
        Some(agents) if agents.is_empty() => {
            println!("No agents found.");
        }
        Some(agents) => {
            println!("Your agents:");
            println!("{:<20} {:<14} {}", "Name", "ID", "");
            println!("{}", "-".repeat(40));
            for agent in agents {
                let name = agent.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let id = agent.get("agent_id").and_then(|v| v.as_str()).unwrap_or("?");
                let is_self = agent.get("is_self").and_then(|v| v.as_bool()).unwrap_or(false);
                let marker = if is_self { " (you)" } else { "" };
                println!("{:<20} {:<14}{}", name, id, marker);
            }
            println!("\nTo send a message: b3 hive send <agent-name> \"your message\"");
        }
        None => {
            println!("Unexpected response format");
        }
    }

    Ok(())
}

/// List conversation rooms.
pub async fn rooms() -> anyhow::Result<()> {
    let (config, client) = build_client()?;

    let url = format!("{}/api/hive/rooms", config.api_url);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to list rooms: HTTP {status} — {body}");
    }

    let body: serde_json::Value = resp.json().await?;
    let rooms = body.get("rooms").and_then(|v| v.as_array());

    match rooms {
        Some(rooms) if rooms.is_empty() => {
            println!("No conversation rooms.");
        }
        Some(rooms) => {
            println!("Conversation rooms:");
            for room in rooms {
                let id = room.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let topic = room.get("topic").and_then(|v| v.as_str()).unwrap_or("(no topic)");
                let members = room.get("members").and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                println!("  {id}  [{topic}]  members: {members}");
            }
        }
        None => println!("Unexpected response format"),
    }

    Ok(())
}

/// Show messages in a room.
pub async fn room(room_id: &str) -> anyhow::Result<()> {
    let (config, client) = build_client()?;

    let url = format!("{}/api/hive/rooms/{}/messages", config.api_url, room_id);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to get room messages: HTTP {status} — {body}");
    }

    let body: serde_json::Value = resp.json().await?;
    let messages = body.get("messages").and_then(|v| v.as_array());

    match messages {
        Some(msgs) if msgs.is_empty() => {
            println!("No messages in room.");
        }
        Some(msgs) => {
            for msg in msgs {
                let from = msg.get("from_agent").and_then(|v| v.as_str()).unwrap_or("?");
                let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let ts = msg.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                println!("[{ts}] {from}: {content}");
            }
        }
        None => println!("Unexpected response format"),
    }

    Ok(())
}

/// Send a message to a conversation room.
pub async fn converse(room_id: &str, message: &str) -> anyhow::Result<()> {
    let (config, client) = build_client()?;

    let url = format!("{}/api/hive/rooms/{}/send", config.api_url, room_id);
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .json(&serde_json::json!({ "message": message }))
        .send()
        .await?;

    if resp.status().is_success() {
        println!("Message sent to room");
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to send to room: HTTP {status} — {body}");
    }

    Ok(())
}
