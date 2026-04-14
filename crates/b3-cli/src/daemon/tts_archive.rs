//! On-disk TTS message archive.
//!
//! Stores stitched WAV files and metadata so the browser can replay
//! messages that were generated while it was disconnected.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use b3_common::tts::TtsEntry;
use tokio::sync::RwLock;

const MAX_ENTRIES: usize = 50;
const INDEX_FILE: &str = "index.json";

/// On-disk archive of TTS messages.
pub struct TtsArchive {
    dir: PathBuf,
    index: RwLock<VecDeque<TtsEntry>>,
}

impl TtsArchive {
    /// Open or create a TTS archive at the given directory.
    pub fn open(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();

        // Create directory if needed
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, "Failed to create TTS archive dir");
        }

        // Load existing index
        let index_path = dir.join(INDEX_FILE);
        let entries = if index_path.exists() {
            match std::fs::read_to_string(&index_path) {
                Ok(data) => {
                    match serde_json::from_str::<Vec<TtsEntry>>(&data) {
                        Ok(vec) => {
                            tracing::info!(entries = vec.len(), "Loaded TTS archive index");
                            VecDeque::from(vec)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to parse TTS archive index");
                            VecDeque::new()
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to read TTS archive index");
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

    /// Store a TTS entry with its WAV audio data.
    pub async fn store(&self, entry: TtsEntry, wav_bytes: &[u8]) -> anyhow::Result<()> {
        let wav_path = self.dir.join(format!("{}.wav", &entry.msg_id));

        // Write WAV file
        tokio::fs::write(&wav_path, wav_bytes).await?;

        let mut index = self.index.write().await;

        // Evict oldest if at capacity
        while index.len() >= MAX_ENTRIES {
            if let Some(old) = index.pop_front() {
                let old_path = self.dir.join(format!("{}.wav", &old.msg_id));
                if let Err(e) = tokio::fs::remove_file(&old_path).await {
                    tracing::warn!(msg_id = %old.msg_id, error = %e, "Failed to remove evicted WAV");
                }
            }
        }

        // Add new entry
        index.push_back(entry);

        // Save index atomically
        self.save_index_locked(&index).await;

        Ok(())
    }

    /// List all entries, newest first.
    pub async fn list(&self) -> Vec<TtsEntry> {
        let index = self.index.read().await;
        let mut entries: Vec<TtsEntry> = index.iter().cloned().collect();
        entries.reverse(); // newest first
        entries
    }

    /// Get the path to a WAV file by message ID.
    pub async fn audio_path(&self, msg_id: &str) -> Option<PathBuf> {
        let index = self.index.read().await;
        if index.iter().any(|e| e.msg_id == msg_id) {
            let path = self.dir.join(format!("{msg_id}.wav"));
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    /// Check if a message ID already exists in the archive.
    pub async fn contains(&self, msg_id: &str) -> bool {
        let index = self.index.read().await;
        index.iter().any(|e| e.msg_id == msg_id)
    }

    /// Save index to disk atomically (write to .tmp, rename).
    async fn save_index_locked(&self, index: &VecDeque<TtsEntry>) {
        let entries: Vec<&TtsEntry> = index.iter().collect();
        let json = match serde_json::to_string_pretty(&entries) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize TTS archive index");
                return;
            }
        };

        let tmp_path = self.dir.join(format!("{INDEX_FILE}.tmp"));
        let final_path = self.dir.join(INDEX_FILE);

        if let Err(e) = tokio::fs::write(&tmp_path, &json).await {
            tracing::warn!(error = %e, "Failed to write TTS archive index tmp");
            return;
        }

        if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
            tracing::warn!(error = %e, "Failed to rename TTS archive index");
        }
    }
}
