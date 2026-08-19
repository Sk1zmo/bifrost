//! On-disk persistence: an append-only chat log and a small key-value store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::net::protocol::ChatPayload;

/// Chat lines kept on disk before the log is rotated.
const MAX_LOG_LINES: usize = 20_000;

/// Durable state under `~/.bifrost/data`.
#[derive(Clone, Debug)]
pub struct DataStore {
    root: PathBuf,
    /// Serialises appends, so concurrent writers cannot interleave a line.
    write_lock: Arc<Mutex<()>>,
}

impl DataStore {
    /// Open, creating the directory if needed.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root)
            .await
            .with_context(|| format!("cannot create the data directory {}", root.display()))?;
        Ok(Self {
            root,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    fn chat_log(&self) -> PathBuf {
        self.root.join("chat.jsonl")
    }

    fn kv_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// Append one chat message to the log.
    pub async fn append_chat(&self, message: &ChatPayload) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let line = serde_json::to_string(message).context("cannot serialise a chat message")?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.chat_log())
            .await
            .context("cannot open the chat log")?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }

    /// Load the most recent `limit` messages, oldest first.
    ///
    /// A corrupt line — a half-written record from a power cut — is skipped
    /// rather than failing the load, since one bad line should not cost the
    /// user their whole history.
    pub async fn load_recent_chat(&self, limit: usize) -> Result<Vec<ChatPayload>> {
        let path = self.chat_log();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = tokio::fs::File::open(&path).await?;
        let mut lines = BufReader::new(file).lines();
        let mut messages: std::collections::VecDeque<ChatPayload> =
            std::collections::VecDeque::with_capacity(limit);
        let mut skipped = 0usize;

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ChatPayload>(&line) {
                Ok(message) => {
                    if messages.len() == limit {
                        messages.pop_front();
                    }
                    messages.push_back(message);
                }
                Err(_) => skipped += 1,
            }
        }

        if skipped > 0 {
            tracing::warn!(skipped, "skipped unreadable lines in the chat log");
        }
        Ok(messages.into())
    }

    /// Trim the log to its most recent [`MAX_LOG_LINES`] lines.
    pub async fn rotate_chat_log(&self) -> Result<()> {
        let path = self.chat_log();
        if !path.exists() {
            return Ok(());
        }

        let _guard = self.write_lock.lock().await;
        let file = tokio::fs::File::open(&path).await?;
        let mut lines = BufReader::new(file).lines();
        let mut kept: std::collections::VecDeque<String> =
            std::collections::VecDeque::with_capacity(MAX_LOG_LINES);

        let mut total = 0usize;
        while let Some(line) = lines.next_line().await? {
            total += 1;
            if kept.len() == MAX_LOG_LINES {
                kept.pop_front();
            }
            kept.push_back(line);
        }

        if total <= MAX_LOG_LINES {
            return Ok(());
        }

        // Rewrite via a temporary file so an interrupted rotation cannot
        // truncate the log to nothing.
        let temp = path.with_extension("jsonl.tmp");
        let mut out = tokio::fs::File::create(&temp).await?;
        for line in &kept {
            out.write_all(line.as_bytes()).await?;
            out.write_all(b"\n").await?;
        }
        out.flush().await?;
        drop(out);
        tokio::fs::rename(&temp, &path).await?;

        tracing::info!(removed = total - kept.len(), "rotated the chat log");
        Ok(())
    }

    /// Read the whole key-value map.
    pub async fn load_state(&self) -> Result<HashMap<String, serde_json::Value>> {
        let path = self.kv_path();
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let raw = tokio::fs::read_to_string(&path).await?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    /// Store one value, replacing any previous one.
    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut state = {
            let path = self.kv_path();
            if path.exists() {
                serde_json::from_str(&tokio::fs::read_to_string(&path).await?).unwrap_or_default()
            } else {
                HashMap::new()
            }
        };

        state.insert(key.to_string(), serde_json::to_value(value)?);

        let path = self.kv_path();
        let temp = path.with_extension("json.tmp");
        tokio::fs::write(&temp, serde_json::to_string_pretty(&state)?).await?;
        tokio::fs::rename(&temp, &path).await?;
        Ok(())
    }

    /// Read one value, or `None` if absent or the wrong shape.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        Ok(self
            .load_state()
            .await?
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::history::compose;

    #[tokio::test]
    async fn chat_appends_and_reloads_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataStore::open(dir.path()).await.unwrap();

        for i in 0..5 {
            store
                .append_chat(&compose("n1", "alice", &format!("m{i}"), None))
                .await
                .unwrap();
        }

        let loaded = store.load_recent_chat(100).await.unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["m0", "m1", "m2", "m3", "m4"]
        );

        // A limit keeps the newest.
        let tail = store.load_recent_chat(2).await.unwrap();
        assert_eq!(
            tail.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["m3", "m4"]
        );
    }

    #[tokio::test]
    async fn a_corrupt_line_does_not_lose_the_rest_of_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataStore::open(dir.path()).await.unwrap();

        store
            .append_chat(&compose("n1", "alice", "before", None))
            .await
            .unwrap();
        // Simulate a torn write from a power cut.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.path().join("chat.jsonl"))
            .await
            .unwrap();
        file.write_all(b"{\"partial\": tru\n").await.unwrap();
        drop(file);
        store
            .append_chat(&compose("n1", "alice", "after", None))
            .await
            .unwrap();

        let loaded = store.load_recent_chat(100).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "before");
        assert_eq!(loaded[1].content, "after");
    }

    #[tokio::test]
    async fn loading_from_an_empty_store_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataStore::open(dir.path()).await.unwrap();

        assert!(store.load_recent_chat(10).await.unwrap().is_empty());
        assert!(store.load_state().await.unwrap().is_empty());
        assert_eq!(store.get::<String>("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn key_value_entries_round_trip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataStore::open(dir.path()).await.unwrap();

        store.put("last_peer", &"alice").await.unwrap();
        store.put("count", &42u32).await.unwrap();
        assert_eq!(
            store.get::<String>("last_peer").await.unwrap().as_deref(),
            Some("alice")
        );
        assert_eq!(store.get::<u32>("count").await.unwrap(), Some(42));

        store.put("count", &43u32).await.unwrap();
        assert_eq!(store.get::<u32>("count").await.unwrap(), Some(43));
        // The other key survives the update.
        assert_eq!(
            store.get::<String>("last_peer").await.unwrap().as_deref(),
            Some("alice")
        );

        // A wrong-typed read is None, not an error.
        assert_eq!(store.get::<u32>("last_peer").await.unwrap(), None);
    }

    #[tokio::test]
    async fn rotation_keeps_the_newest_lines_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataStore::open(dir.path()).await.unwrap();

        // Write past the cap without going through append_chat 20k times.
        let mut bulk = String::new();
        for i in 0..(MAX_LOG_LINES + 100) {
            bulk.push_str(
                &serde_json::to_string(&compose("n1", "a", &format!("m{i}"), None)).unwrap(),
            );
            bulk.push('\n');
        }
        tokio::fs::write(dir.path().join("chat.jsonl"), bulk)
            .await
            .unwrap();

        store.rotate_chat_log().await.unwrap();

        let loaded = store.load_recent_chat(MAX_LOG_LINES + 500).await.unwrap();
        assert_eq!(loaded.len(), MAX_LOG_LINES);
        assert_eq!(
            loaded.last().unwrap().content,
            format!("m{}", MAX_LOG_LINES + 99)
        );
        assert!(!dir.path().join("chat.jsonl.tmp").exists());
    }
}
