//! Session persistence — saves/loads the command index to disk
//! so data survives daemon restarts.
//!
//! Sessions are stored as individual JSON files under `~/.rewind/sessions/`.

use crate::protocol::ShellEvent;
use crate::session::SessionStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, error, info, warn};

/// On-disk representation of a session.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedSession {
    tty: String,
    pid: u32,
    shell: String,
    started_ts: i64,
    active: bool,
    events: Vec<ShellEvent>,
}

pub struct Persistence {
    sessions_dir: PathBuf,
}

impl Persistence {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            sessions_dir: data_dir.join("sessions"),
        }
    }

    /// Ensure the sessions directory exists.
    pub async fn init(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.sessions_dir).await?;
        Ok(())
    }

    /// Persist a single event to the session's append-only log.
    pub async fn append_event(&self, tty: &str, event: &ShellEvent) {
        let filename = tty_to_filename(tty);
        let path = self.sessions_dir.join(&filename);

        let line = match serde_json::to_string(event) {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "failed to serialize event");
                return;
            }
        };

        // Append mode — one JSON line per event
        if let Err(e) = append_line(&path, &line).await {
            error!(path = %path.display(), error = %e, "failed to persist event");
        }
    }

    /// Load all persisted sessions and replay events into the store.
    pub async fn load_all(&self, store: &mut SessionStore) -> usize {
        let mut loaded = 0;

        let mut entries = match fs::read_dir(&self.sessions_dir).await {
            Ok(e) => e,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(error = %e, "failed to read sessions directory");
                }
                return 0;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            match fs::read_to_string(&path).await {
                Ok(contents) => {
                    let mut count = 0;
                    for line in contents.lines() {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<ShellEvent>(line) {
                            Ok(event) => {
                                store.ingest(event);
                                count += 1;
                            }
                            Err(e) => {
                                warn!(
                                    path = %path.display(),
                                    line = %line,
                                    error = %e,
                                    "skipping malformed event"
                                );
                            }
                        }
                    }
                    debug!(
                        path = %path.display(),
                        events = count,
                        "loaded session"
                    );
                    loaded += count;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to read session file");
                }
            }
        }

        info!(sessions = store.sessions.len(), events = loaded, "loaded persisted sessions");
        loaded
    }

    /// Remove session files older than `max_age_days`.
    pub async fn cleanup(&self, max_age_days: u64) {
        let cutoff = std::time::SystemTime::now()
            - std::time::Duration::from_secs(max_age_days * 86400);

        let mut entries = match fs::read_dir(&self.sessions_dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(meta) = entry.metadata().await {
                if let Ok(modified) = meta.modified() {
                    if modified < cutoff {
                        let path = entry.path();
                        info!(path = %path.display(), "removing stale session file");
                        let _ = fs::remove_file(&path).await;
                    }
                }
            }
        }
    }
}

/// Append a line to a file (create if it doesn't exist).
async fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;

    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(())
}

/// Convert a TTY path like `/dev/ttys001` to a safe filename.
fn tty_to_filename(tty: &str) -> String {
    let sanitized: String = tty
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("{sanitized}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tty_to_filename() {
        assert_eq!(
            tty_to_filename("/dev/ttys001"),
            "_dev_ttys001.jsonl"
        );
        assert_eq!(
            tty_to_filename("/dev/pts/3"),
            "_dev_pts_3.jsonl"
        );
    }
}
