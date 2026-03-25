//! In-memory session and command index.
//!
//! Each TTY gets its own `Session`. Commands are appended on `CmdStart`
//! and completed on `CmdEnd`. The index is the source of truth for
//! minimap computation and query responses.

use crate::protocol::{CommandCategory, CommandEntry, SessionSummary, ShellEvent};
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;
use tracing::{debug, warn};
use uuid::Uuid;

/// Top-level store keyed by TTY path.
#[derive(Debug, Default)]
pub struct SessionStore {
    pub sessions: HashMap<String, Session>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub tty: String,
    pub pid: u32,
    pub shell: String,
    pub started_at: DateTime<Utc>,
    pub active: bool,
    pub commands: Vec<Command>,
    /// Maps shell PID to the index of the currently-running command.
    inflight: HashMap<u32, usize>,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub id: String,
    pub cmd: String,
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub output_lines: Option<u32>,
    pub category: CommandCategory,
}

impl Command {
    pub fn duration_ms(&self) -> Option<u64> {
        self.ended_at.map(|end| {
            (end - self.started_at)
                .num_milliseconds()
                .unsigned_abs()
        })
    }

    pub fn to_entry(&self) -> CommandEntry {
        CommandEntry {
            id: self.id.clone(),
            cmd: self.cmd.clone(),
            cwd: self.cwd.clone(),
            started_at: self.started_at,
            ended_at: self.ended_at,
            exit_code: self.exit_code,
            output_lines: self.output_lines,
            duration_ms: self.duration_ms(),
            category: self.category.clone(),
        }
    }
}

impl Session {
    pub fn new(tty: String, pid: u32, shell: String, ts: i64) -> Self {
        Self {
            tty,
            pid,
            shell,
            started_at: Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now),
            active: true,
            commands: Vec::new(),
            inflight: HashMap::new(),
        }
    }

    pub fn to_summary(&self) -> SessionSummary {
        SessionSummary {
            tty: self.tty.clone(),
            pid: self.pid,
            shell: self.shell.clone(),
            started_at: self.started_at,
            command_count: self.commands.len(),
            active: self.active,
        }
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process an incoming shell event and update the index.
    pub fn ingest(&mut self, event: ShellEvent) {
        match event {
            ShellEvent::SessionStart {
                tty,
                pid,
                shell,
                ts,
            } => {
                debug!(tty = %tty, pid, shell = %shell, "new session");
                let session = Session::new(tty.clone(), pid, shell, ts);
                self.sessions.insert(tty, session);
            }

            ShellEvent::SessionEnd { tty, ts, .. } => {
                if let Some(session) = self.sessions.get_mut(&tty) {
                    debug!(tty = %tty, "session ended");
                    session.active = false;
                    // Complete any inflight commands
                    let inflight_idxs: Vec<usize> =
                        session.inflight.values().copied().collect();
                    for idx in inflight_idxs {
                        if let Some(cmd) = session.commands.get_mut(idx) {
                            if cmd.ended_at.is_none() {
                                cmd.ended_at =
                                    Utc.timestamp_opt(ts, 0).single();
                                cmd.exit_code = Some(-1); // killed
                            }
                        }
                    }
                    session.inflight.clear();
                } else {
                    warn!(tty = %tty, "session_end for unknown session");
                }
            }

            ShellEvent::CmdStart {
                cmd,
                cwd,
                ts,
                pid,
                tty,
            } => {
                let session = self
                    .sessions
                    .entry(tty.clone())
                    .or_insert_with(|| {
                        debug!(tty = %tty, "auto-creating session from cmd_start");
                        Session::new(tty.clone(), pid, "unknown".into(), ts)
                    });

                let category = categorize_command(&cmd);
                let command = Command {
                    id: Uuid::new_v4().to_string(),
                    cmd: cmd.clone(),
                    cwd,
                    started_at: Utc
                        .timestamp_opt(ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    ended_at: None,
                    exit_code: None,
                    output_lines: None,
                    category,
                };

                let idx = session.commands.len();
                debug!(tty = %tty, cmd = %cmd, id = %command.id, "cmd_start");
                session.commands.push(command);
                session.inflight.insert(pid, idx);
            }

            ShellEvent::CmdEnd {
                exit_code,
                ts,
                pid,
                tty,
                output_lines,
            } => {
                if let Some(session) = self.sessions.get_mut(&tty) {
                    if let Some(idx) = session.inflight.remove(&pid) {
                        if let Some(cmd) = session.commands.get_mut(idx) {
                            cmd.ended_at =
                                Utc.timestamp_opt(ts, 0).single();
                            cmd.exit_code = Some(exit_code);
                            cmd.output_lines = output_lines;

                            // Upgrade category if it was an error
                            if exit_code != 0
                                && cmd.category == CommandCategory::General
                            {
                                cmd.category = CommandCategory::Error;
                            }
                            // Mark long-running (>10s)
                            if let Some(dur) = cmd.duration_ms() {
                                if dur > 10_000
                                    && cmd.category == CommandCategory::General
                                {
                                    cmd.category = CommandCategory::LongRunning;
                                }
                            }

                            debug!(
                                tty = %tty,
                                cmd = %cmd.cmd,
                                exit_code,
                                duration_ms = ?cmd.duration_ms(),
                                "cmd_end"
                            );
                        }
                    } else {
                        warn!(tty = %tty, pid, "cmd_end with no inflight command");
                    }
                } else {
                    warn!(tty = %tty, "cmd_end for unknown session");
                }
            }
        }
    }

    /// Get all commands for a TTY session.
    pub fn get_commands(&self, tty: &str) -> Option<Vec<CommandEntry>> {
        self.sessions
            .get(tty)
            .map(|s| s.commands.iter().map(|c| c.to_entry()).collect())
    }

    /// List all sessions.
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.sessions.values().map(|s| s.to_summary()).collect()
    }
}

/// Heuristic command categorization based on the command string.
fn categorize_command(cmd: &str) -> CommandCategory {
    let cmd_lower = cmd.to_lowercase();
    let first_word = cmd_lower.split_whitespace().next().unwrap_or("");

    // Check for pipe chains — categorize by the first command
    let base_cmd = first_word
        .rsplit('/')
        .next()
        .unwrap_or(first_word);

    match base_cmd {
        // Test runners
        _ if is_test_command(&cmd_lower) => CommandCategory::Test,
        // Git
        "git" | "gh" | "hub" => CommandCategory::Git,
        // Docker
        "docker" | "docker-compose" | "podman" | "nerdctl" => CommandCategory::Docker,
        // SSH / remote
        "ssh" | "scp" | "rsync" | "mosh" => CommandCategory::Ssh,
        // Build tools
        _ if is_build_command(&cmd_lower) => CommandCategory::Build,
        _ => CommandCategory::General,
    }
}

fn is_test_command(cmd: &str) -> bool {
    let patterns = [
        "npm test",
        "yarn test",
        "pnpm test",
        "cargo test",
        "go test",
        "pytest",
        "python -m pytest",
        "jest",
        "vitest",
        "mocha",
        "rspec",
        "mix test",
        "zig test",
        "make test",
        "gradle test",
        "mvn test",
    ];
    patterns.iter().any(|p| cmd.contains(p))
}

fn is_build_command(cmd: &str) -> bool {
    let patterns = [
        "npm run build",
        "yarn build",
        "cargo build",
        "go build",
        "make",
        "cmake",
        "gradle build",
        "mvn package",
        "zig build",
        "webpack",
        "vite build",
        "tsc",
        "gcc",
        "g++",
        "clang",
        "rustc",
        "javac",
    ];
    patterns.iter().any(|p| cmd.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_categorize() {
        assert_eq!(categorize_command("git push origin main"), CommandCategory::Git);
        assert_eq!(categorize_command("cargo test --release"), CommandCategory::Test);
        assert_eq!(categorize_command("docker build -t app ."), CommandCategory::Docker);
        assert_eq!(categorize_command("ssh user@host"), CommandCategory::Ssh);
        assert_eq!(categorize_command("npm run build"), CommandCategory::Build);
        assert_eq!(categorize_command("ls -la"), CommandCategory::General);
    }

    #[test]
    fn test_session_lifecycle() {
        let mut store = SessionStore::new();

        store.ingest(ShellEvent::SessionStart {
            tty: "/dev/ttys001".into(),
            pid: 1234,
            shell: "zsh".into(),
            ts: 1700000000,
        });

        store.ingest(ShellEvent::CmdStart {
            cmd: "npm test".into(),
            cwd: "/home/user/project".into(),
            ts: 1700000010,
            pid: 1234,
            tty: "/dev/ttys001".into(),
        });

        store.ingest(ShellEvent::CmdEnd {
            exit_code: 0,
            ts: 1700000015,
            pid: 1234,
            tty: "/dev/ttys001".into(),
            output_lines: Some(42),
        });

        let cmds = store.get_commands("/dev/ttys001").unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].cmd, "npm test");
        assert_eq!(cmds[0].exit_code, Some(0));
        assert_eq!(cmds[0].output_lines, Some(42));
        assert_eq!(cmds[0].category, CommandCategory::Test);
        assert_eq!(cmds[0].duration_ms, Some(5000));
    }
}
