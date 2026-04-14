//! On-disk info panel archive.
//!
//! Stores HTML content pushed by the agent via voice_share_info().
//! Persists across browser sessions and devices — browsers fetch from
//! the daemon on load instead of relying on localStorage.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const MAX_ENTRIES: usize = 1000;
const INDEX_FILE: &str = "info-archive.json";

/// A single info entry — HTML content pushed by voice_share_info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoEntry {
    /// Millisecond timestamp when the entry was created.
    pub ts: u64,
    /// Short title extracted from the HTML (first heading or text).
    pub title: String,
    /// Full HTML content.
    pub html: String,
}

/// Lightweight entry for listing (no HTML body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoEntrySummary {
    pub ts: u64,
    pub title: String,
    pub index: usize,
}

/// On-disk archive of info panel entries.
pub struct InfoArchive {
    dir: PathBuf,
    index: RwLock<VecDeque<InfoEntry>>,
}

impl InfoArchive {
    /// Open or create an info archive at the given directory.
    pub fn open(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();

        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, "Failed to create info archive dir");
        }

        let index_path = dir.join(INDEX_FILE);
        let entries = if index_path.exists() {
            match std::fs::read_to_string(&index_path) {
                Ok(data) => match serde_json::from_str::<Vec<InfoEntry>>(&data) {
                    Ok(vec) => {
                        tracing::info!(entries = vec.len(), "Loaded info archive");
                        VecDeque::from(vec)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to parse info archive");
                        VecDeque::new()
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to read info archive");
                    VecDeque::new()
                }
            }
        } else {
            VecDeque::new()
        };

        Self {
            dir,
            index: RwLock::new(entries),
        }
    }

    /// Store a new info entry. Evicts oldest if at capacity.
    pub async fn store(&self, title: String, html: String) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let entry = InfoEntry { ts, title, html };

        let mut index = self.index.write().await;

        // Evict oldest if at capacity
        while index.len() >= MAX_ENTRIES {
            index.pop_front();
        }

        index.push_back(entry);

        // Persist to disk
        let entries: Vec<&InfoEntry> = index.iter().collect();
        let index_path = self.dir.join(INDEX_FILE);
        if let Err(e) = std::fs::write(&index_path, serde_json::to_string(&entries).unwrap_or_default()) {
            tracing::warn!(error = %e, "Failed to write info archive");
        }
    }

    /// List all entries (summary only — no HTML body).
    pub async fn list(&self) -> Vec<InfoEntrySummary> {
        let index = self.index.read().await;
        index
            .iter()
            .enumerate()
            .rev() // newest first
            .map(|(i, e)| InfoEntrySummary {
                ts: e.ts,
                title: e.title.clone(),
                index: i,
            })
            .collect()
    }

    /// Get a specific entry by index (0 = oldest).
    pub async fn get(&self, idx: usize) -> Option<InfoEntry> {
        let index = self.index.read().await;
        index.get(idx).cloned()
    }

    /// Get the latest entry.
    pub async fn latest(&self) -> Option<InfoEntry> {
        let index = self.index.read().await;
        index.back().cloned()
    }

    /// Total number of entries.
    pub async fn len(&self) -> usize {
        self.index.read().await.len()
    }
}
