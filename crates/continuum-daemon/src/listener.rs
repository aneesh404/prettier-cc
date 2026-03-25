//! Unix socket listeners for ingestion (shell events) and queries (UI clients).

use crate::protocol::{Query, Response, ShellEvent};
use crate::minimap::MinimapBuilder;
use crate::persistence::Persistence;
use crate::session::SessionStore;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Shared daemon state.
pub type SharedStore = Arc<RwLock<SessionStore>>;
pub type SharedPersistence = Arc<Persistence>;

// ── Ingestion listener ──────────────────────────────────────────────────────

/// Start the ingestion socket listener. Shell hooks connect here
/// and send newline-delimited JSON events.
pub async fn start_ingest_listener(
    socket_path: &Path,
    store: SharedStore,
    persistence: SharedPersistence,
) -> std::io::Result<()> {
    // Remove stale socket
    let _ = tokio::fs::remove_file(socket_path).await;

    let listener = UnixListener::bind(socket_path)?;
    info!(path = %socket_path.display(), "ingestion listener started");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let store = Arc::clone(&store);
                let persistence = Arc::clone(&persistence);
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_ingest_connection(stream, store, persistence).await
                    {
                        debug!(error = %e, "ingest connection ended");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "failed to accept ingest connection");
            }
        }
    }
}

async fn handle_ingest_connection(
    stream: UnixStream,
    store: SharedStore,
    persistence: SharedPersistence,
) -> std::io::Result<()> {
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<ShellEvent>(&line) {
            Ok(event) => {
                let tty = match &event {
                    ShellEvent::CmdStart { tty, .. }
                    | ShellEvent::CmdEnd { tty, .. }
                    | ShellEvent::SessionStart { tty, .. }
                    | ShellEvent::SessionEnd { tty, .. } => tty.clone(),
                };

                // Persist first (append-only log)
                persistence.append_event(&tty, &event).await;

                // Then update in-memory index
                let mut store = store.write().await;
                store.ingest(event);
            }
            Err(e) => {
                warn!(error = %e, line = %line, "malformed ingest event");
            }
        }
    }

    Ok(())
}

// ── Query listener ──────────────────────────────────────────────────────────

/// Start the query socket listener. UI clients connect here
/// and send JSON queries, receiving JSON responses.
pub async fn start_query_listener(
    socket_path: &Path,
    store: SharedStore,
) -> std::io::Result<()> {
    // Remove stale socket
    let _ = tokio::fs::remove_file(socket_path).await;

    let listener = UnixListener::bind(socket_path)?;
    info!(path = %socket_path.display(), "query listener started");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    if let Err(e) = handle_query_connection(stream, store).await {
                        debug!(error = %e, "query connection ended");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "failed to accept query connection");
            }
        }
    }
}

async fn handle_query_connection(
    stream: UnixStream,
    store: SharedStore,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let reader = BufReader::new(reader);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Query>(&line) {
            Ok(query) => {
                let store = store.read().await;
                handle_query(&store, query)
            }
            Err(e) => Response::Error {
                message: format!("malformed query: {e}"),
            },
        };

        let mut resp_json = serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"type":"error","message":"serialization failed"}"#.into());
        resp_json.push('\n');

        if let Err(e) = writer.write_all(resp_json.as_bytes()).await {
            debug!(error = %e, "failed to write query response");
            break;
        }
    }

    Ok(())
}

fn handle_query(store: &SessionStore, query: Query) -> Response {
    match query {
        Query::ListSessions => Response::Sessions {
            sessions: store.list_sessions(),
        },

        Query::GetSession { tty } => match store.get_commands(&tty) {
            Some(commands) => Response::Session { commands },
            None => Response::Error {
                message: format!("unknown session: {tty}"),
            },
        },

        Query::GetMinimap { tty, height } => {
            let builder = MinimapBuilder::new(store);
            match builder.build(&tty, height, 0.8, 0.2) {
                Some((segments, viewport)) => Response::Minimap { segments, viewport },
                None => Response::Error {
                    message: format!("unknown session: {tty}"),
                },
            }
        }

        Query::GetPeek { tty, command_id } => {
            // Peek returns metadata about the command — actual terminal content
            // would need to come from the scrollback buffer (future integration).
            match store.get_commands(&tty) {
                Some(commands) => {
                    if let Some(cmd) = commands.iter().find(|c| c.id == command_id) {
                        let lines = vec![
                            format!("$ {}", cmd.cmd),
                            format!(
                                "  cwd: {}",
                                cmd.cwd
                            ),
                            format!(
                                "  exit: {}",
                                cmd.exit_code
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "running…".into())
                            ),
                            format!(
                                "  duration: {}",
                                cmd.duration_ms
                                    .map(|d| format!("{:.1}s", d as f64 / 1000.0))
                                    .unwrap_or_else(|| "—".into())
                            ),
                            format!(
                                "  output: ~{} lines",
                                cmd.output_lines.unwrap_or(0)
                            ),
                        ];
                        Response::Peek { command_id, lines }
                    } else {
                        Response::Error {
                            message: format!("unknown command: {command_id}"),
                        }
                    }
                }
                None => Response::Error {
                    message: format!("unknown session: {tty}"),
                },
            }
        }
    }
}
