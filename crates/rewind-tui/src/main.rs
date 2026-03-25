mod chat;
mod embedded;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify::{recommended_watcher, RecursiveMode, Watcher};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph},
    Terminal,
};
use rewind_core::protocol::{SessionInfo, Turn};
use rewind_core::transcript::{parse_transcript, parse_transcript_meta};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

// ── Data types ──────────────────────────────────────────────────────────────

struct ProjectEntry {
    display_name: String,
    folder_name: String,
    path: PathBuf,
    session_count: usize,
    total_prompts: usize,
    total_tools: usize,
    latest_modified: std::time::SystemTime,
}

struct SessionEntry {
    path: PathBuf,
    session_id: String,
    info: SessionInfo,
}

struct AgentInstance {
    term: embedded::EmbeddedTerm,
    session_id: String,
    label: String,
    started_at: std::time::Instant,
    forked: bool,
    dangerous: bool,
}

#[derive(Clone)]
struct PinnedItem {
    project_path: PathBuf,
    transcript_path: PathBuf,
    session_id: String,
    turn_index: usize, // Turn.index (1-based)
    label: String,     // short prompt preview
}

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq)]
enum LeftPaneMode {
    Sessions,
    Agents,
}

#[derive(Clone)]
enum Screen {
    Projects,
    /// Split view: sessions on left, timeline preview on right
    SessionSplit { project_idx: usize, focus: Pane },
    /// Full-screen timeline (entered from split with Enter on right pane)
    Timeline,
    /// Embedded Claude Code: timeline on left, live terminal on right
    Embedded { focus: Pane },
}

// ── App State ───────────────────────────────────────────────────────────────

struct App {
    screen: Screen,
    should_quit: bool,
    last_error: Option<String>,

    // Projects
    projects: Vec<ProjectEntry>,
    projects_state: ListState,

    // Sessions (left pane of split)
    sessions: Vec<SessionEntry>,
    sessions_state: ListState,

    // Timeline / preview (right pane of split, or full screen)
    info: Option<SessionInfo>,
    turns: Vec<Turn>,
    timeline_state: ListState,
    expanded: Vec<bool>,
    transcript_path: Option<PathBuf>,

    // Track which session index is currently previewed on the right
    previewed_session: Option<usize>,

    // Multi-agent support: multiple running Claude Code instances
    agents: Vec<AgentInstance>,
    agents_state: ListState,
    active_agent_idx: Option<usize>,

    // Left pane carousel mode
    left_pane_mode: LeftPaneMode,

    // Claude Code version info (fetched once at startup)
    claude_version: String,

    // Activity heatmap: days mapped to prompt counts (last ~12 weeks)
    activity_map: std::collections::BTreeMap<String, usize>, // "YYYY-MM-DD" → count

    // Project search
    searching: bool,
    search_query: String,
    filtered_indices: Vec<usize>, // indices into `projects` that match the query

    // Timeline search
    timeline_searching: bool,
    timeline_search_query: String,
    timeline_filtered_indices: Vec<usize>, // indices into `turns` that match

    // Double-Esc detection: timestamp of last Esc press
    last_esc_time: Option<std::time::Instant>,

    // Pinned prompts (accessible from any screen via 1-9 keys)
    pins: Vec<PinnedItem>,

    // Danger mode: adds --dangerously-skip-permissions to spawned agents
    danger_mode: bool,

    // Expanded project card on Projects screen (shows extra details)
    project_expanded: bool,

    // Cached area of the embedded terminal widget (set during render, used for mouse coords)
    embedded_term_area: ratatui::layout::Rect,

    // Agent command palette (Spotlight-style overlay)
    agent_palette_open: bool,
    agent_palette_state: ListState,
}

impl App {
    fn new() -> Self {
        let mut app = App {
            screen: Screen::Projects,
            should_quit: false,
            last_error: None,
            projects: Vec::new(),
            projects_state: ListState::default(),
            sessions: Vec::new(),
            sessions_state: ListState::default(),
            info: None,
            turns: Vec::new(),
            timeline_state: ListState::default(),
            expanded: Vec::new(),
            transcript_path: None,
            previewed_session: None,
            agents: Vec::new(),
            agents_state: ListState::default(),
            active_agent_idx: None,
            left_pane_mode: LeftPaneMode::Sessions,
            claude_version: get_claude_version(),
            activity_map: std::collections::BTreeMap::new(),
            searching: false,
            search_query: String::new(),
            filtered_indices: Vec::new(),
            timeline_searching: false,
            timeline_search_query: String::new(),
            timeline_filtered_indices: Vec::new(),
            last_esc_time: None,
            pins: Vec::new(),
            danger_mode: false,
            project_expanded: false,
            embedded_term_area: ratatui::layout::Rect::default(),
            agent_palette_open: false,
            agent_palette_state: ListState::default(),
        };
        app.load_projects();
        if !app.projects.is_empty() {
            app.projects_state.select(Some(0));
        }
        app
    }

    fn new_with_transcript(path: PathBuf) -> Self {
        let mut app = App::new();
        app.load_transcript(&path);
        app.screen = Screen::Timeline;
        app
    }

    fn load_projects(&mut self) {
        let base = match dirs::home_dir() {
            Some(h) => h.join(".claude/projects"),
            None => return,
        };

        let mut projects = Vec::new();
        let mut daily_activity: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let mut session_count = 0;
                let mut total_prompts = 0usize;
                let mut total_tools = 0usize;
                let mut latest = std::time::SystemTime::UNIX_EPOCH;
                if let Ok(files) = std::fs::read_dir(&path) {
                    for f in files.flatten() {
                        let fp = f.path();
                        if fp.extension().and_then(|e| e.to_str()) == Some("jsonl")
                            && !fp.to_string_lossy().contains("subagents")
                        {
                            session_count += 1;
                            if let Ok(m) = fp.metadata().and_then(|m| m.modified()) {
                                if m > latest {
                                    latest = m;
                                }
                                // Bucket by date for heatmap
                                if let Ok(dur) = m.duration_since(std::time::UNIX_EPOCH) {
                                    let secs = dur.as_secs() as i64;
                                    let date_str = epoch_to_date_str(secs);
                                    *daily_activity.entry(date_str).or_insert(0) += 1;
                                }
                            }
                            // Collect prompt/tool counts from meta
                            if let Ok(meta) = parse_transcript_meta(&fp) {
                                total_prompts += meta.prompt_count;
                                total_tools += meta.tool_call_count;
                            }
                        }
                    }
                }
                if session_count > 0 {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let display_name = name.replace('-', "/");
                    let display_name = display_name
                        .strip_prefix('/')
                        .unwrap_or(&display_name)
                        .to_string();
                    // Extract folder name (last path component)
                    let folder_name = display_name.split('/').last().unwrap_or(&display_name).to_string();
                    projects.push(ProjectEntry {
                        display_name,
                        folder_name,
                        path,
                        session_count,
                        total_prompts,
                        total_tools,
                        latest_modified: latest,
                    });
                }
            }
        }
        projects.sort_by(|a, b| b.latest_modified.cmp(&a.latest_modified));
        self.projects = projects;
        self.activity_map = daily_activity;
    }

    fn load_sessions(&mut self, project_idx: usize) {
        let project = &self.projects[project_idx];
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&project.path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && !p.to_string_lossy().contains("subagents")
            })
            .collect();

        paths.sort_by(|a, b| {
            let ma = a.metadata().and_then(|m| m.modified()).ok();
            let mb = b.metadata().and_then(|m| m.modified()).ok();
            mb.cmp(&ma)
        });

        let mut sessions = Vec::new();
        for path in paths.iter().take(20) {
            // Use lightweight meta parser — avoids loading full transcript into memory
            if let Ok(info) = parse_transcript_meta(path) {
                let session_id = if info.session_id.len() > 8 {
                    info.session_id[..8].to_string()
                } else {
                    info.session_id.clone()
                };
                sessions.push(SessionEntry {
                    path: path.clone(),
                    session_id,
                    info,
                });
            }
        }

        self.sessions = sessions;
        self.sessions_state = ListState::default();
        if !self.sessions.is_empty() {
            self.sessions_state.select(Some(0));
        }
        self.previewed_session = None;
        self.screen = Screen::SessionSplit {
            project_idx,
            focus: Pane::Left,
        };
        // Load preview for first session
        self.update_preview();
    }

    fn update_preview(&mut self) {
        let sel = self.sessions_state.selected();
        if sel == self.previewed_session {
            return; // already showing this one
        }
        self.previewed_session = sel;
        if let Some(i) = sel {
            if i < self.sessions.len() {
                self.load_transcript(&self.sessions[i].path.clone());
            }
        }
    }

    fn load_transcript(&mut self, path: &PathBuf) {
        match parse_transcript(path) {
            Ok((info, turns)) => {
                self.expanded = vec![false; turns.len()];
                self.timeline_state = ListState::default();
                if !turns.is_empty() {
                    self.timeline_state.select(Some(turns.len() - 1));
                }
                self.info = Some(info);
                self.turns = turns;
                self.transcript_path = Some(path.clone());
                self.last_error = None;
                self.timeline_searching = false;
                self.timeline_search_query.clear();
                self.timeline_filtered_indices.clear();
            }
            Err(e) => {
                self.info = None;
                self.turns.clear();
                self.expanded.clear();
                self.last_error = Some(e);
            }
        }
    }

    /// Update filtered project indices based on search query.
    fn update_search_filter(&mut self) {
        let q = self.search_query.to_lowercase();
        if q.is_empty() {
            self.filtered_indices = (0..self.projects.len()).collect();
        } else {
            self.filtered_indices = self.projects.iter().enumerate()
                .filter(|(_, p)| {
                    p.display_name.to_lowercase().contains(&q)
                        || p.folder_name.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();
        }
        // Reset selection to first match
        self.projects_state.select(if self.filtered_indices.is_empty() { None } else { Some(0) });
    }

    /// Get the real project index for the currently selected item in filtered view.
    fn selected_project_idx(&self) -> Option<usize> {
        self.projects_state.selected()
            .and_then(|sel| {
                if self.searching || !self.filtered_indices.is_empty() {
                    self.filtered_indices.get(sel).copied()
                } else {
                    Some(sel)
                }
            })
    }

    /// Update filtered timeline indices based on search query.
    fn update_timeline_search_filter(&mut self) {
        let q = self.timeline_search_query.to_lowercase();
        if q.is_empty() {
            self.timeline_filtered_indices = (0..self.turns.len()).collect();
        } else {
            self.timeline_filtered_indices = self.turns.iter().enumerate()
                .filter(|(_, t)| {
                    t.prompt_text.to_lowercase().contains(&q)
                        || t.tool_calls.iter().any(|tc|
                            tc.name.to_lowercase().contains(&q)
                            || tc.summary.to_lowercase().contains(&q))
                })
                .map(|(i, _)| i)
                .collect();
        }
        self.timeline_state.select(if self.timeline_filtered_indices.is_empty() { None } else { Some(0) });
    }

    /// Get the real turn index for the currently selected timeline item in filtered view.
    fn selected_turn_real_idx(&self) -> Option<usize> {
        let sel = self.timeline_state.selected()?;
        if self.timeline_searching || !self.timeline_search_query.is_empty() {
            self.timeline_filtered_indices.get(sel).copied()
        } else {
            Some(sel)
        }
    }

    /// Toggle a pin for the currently selected turn. Returns true if pinned, false if unpinned.
    fn toggle_pin(&mut self) -> Option<bool> {
        let transcript_path = self.transcript_path.as_ref()?.clone();
        let info = self.info.as_ref()?;
        let project_path = {
            // Find the project folder path from the transcript path's parent
            transcript_path.parent()?.to_path_buf()
        };

        // Get the real turn index
        let turn_i = if self.timeline_searching || !self.timeline_search_query.is_empty() {
            self.selected_turn_real_idx()?
        } else {
            self.timeline_state.selected()?
        };
        let turn = self.turns.get(turn_i)?;
        let turn_index = turn.index;

        // Check if already pinned — if so, unpin
        if let Some(pos) = self.pins.iter().position(|p| {
            p.transcript_path == transcript_path && p.turn_index == turn_index
        }) {
            self.pins.remove(pos);
            return Some(false);
        }

        // Max 9 pins
        if self.pins.len() >= 9 {
            return None;
        }

        let label = if turn.prompt_text.len() > 25 {
            format!("{}…", &turn.prompt_text[..24])
        } else {
            turn.prompt_text.clone()
        };

        self.pins.push(PinnedItem {
            project_path,
            transcript_path,
            session_id: info.session_id.clone(),
            turn_index,
            label,
        });
        Some(true)
    }

    /// Jump to a pinned item by pin index (0-based). Returns the Screen to navigate to.
    fn jump_to_pin(&mut self, pin_idx: usize) -> bool {
        if pin_idx >= self.pins.len() {
            return false;
        }
        let pin = self.pins[pin_idx].clone();

        // Find the project
        let project_idx = match self.projects.iter().position(|p| p.path == pin.project_path) {
            Some(i) => i,
            None => return false,
        };

        // Load sessions for that project
        self.load_sessions(project_idx);

        // Find and select the right session
        let session_idx = self.sessions.iter().position(|s| s.path == pin.transcript_path);
        if let Some(si) = session_idx {
            self.sessions_state.select(Some(si));
            self.previewed_session = None; // force reload
            self.load_transcript(&pin.transcript_path);
        }

        // Find and select the right turn
        let turn_idx = self.turns.iter().position(|t| t.index == pin.turn_index);
        if let Some(ti) = turn_idx {
            self.timeline_state.select(Some(ti));
        }

        // Set screen to SessionSplit with right pane focus
        self.screen = Screen::SessionSplit { project_idx, focus: Pane::Right };
        true
    }

    fn reload_timeline(&mut self) {
        if let Some(ref path) = self.transcript_path.clone() {
            let old_len = self.turns.len();
            let was_at_bottom = self
                .timeline_state
                .selected()
                .map(|s| s >= old_len.saturating_sub(1))
                .unwrap_or(true);

            self.load_transcript(path);

            if was_at_bottom && !self.turns.is_empty() {
                self.timeline_state.select(Some(self.turns.len() - 1));
            }
        }
    }

    /// Get the full session ID for the currently previewed session.
    fn current_session_id(&self) -> Option<String> {
        self.info.as_ref().map(|i| i.session_id.clone())
    }

    /// Get the turn index for the selected timeline item (1-based, matching Claude Code's checkpoint numbering).
    fn selected_turn_index(&self) -> Option<usize> {
        self.timeline_state
            .selected()
            .and_then(|i| self.turns.get(i))
            .map(|t| t.index)
    }
}

/// Spawn Claude Code in an embedded PTY and add it to the agents list.
fn open_embedded(app: &mut App, fork: bool, dangerous: bool, rows: u16, cols: u16) {
    let info = match app.info.as_ref() {
        Some(i) => i.clone(),
        None => {
            app.last_error = Some("No session loaded".into());
            return;
        }
    };

    let mut args: Vec<String> = vec![
        "--resume".into(),
        info.session_id.clone(),
    ];
    if fork {
        args.push("--fork-session".into());
    }
    if dangerous {
        args.push("--dangerously-skip-permissions".into());
    }

    let sid_short = if info.session_id.len() > 8 {
        info.session_id[..8].to_string()
    } else {
        info.session_id.clone()
    };
    let danger_tag = if dangerous { " ⚠" } else { "" };
    let label = if fork {
        format!("Fork {sid_short}{danger_tag}")
    } else {
        format!("Resume {sid_short}{danger_tag}")
    };

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let cwd = std::path::PathBuf::from(&info.project_dir);

    match embedded::EmbeddedTerm::spawn("claude", &arg_refs, rows, cols, Some(&cwd)) {
        Ok(term) => {
            let agent = AgentInstance {
                term,
                session_id: info.session_id.clone(),
                label,
                started_at: std::time::Instant::now(),
                forked: fork,
                dangerous,
            };
            app.agents.push(agent);
            let idx = app.agents.len() - 1;
            app.active_agent_idx = Some(idx);
            app.agents_state.select(Some(idx));
            app.screen = Screen::Embedded { focus: Pane::Right };
            app.last_error = None;
        }
        Err(e) => {
            app.last_error = Some(format!("Failed to start Claude: {e}"));
        }
    }
}

// ── Display helpers ─────────────────────────────────────────────────────────

fn tool_icon(name: &str) -> &str {
    match name {
        "Write" => "📝",
        "Read" => "📖",
        "Edit" => "✏️ ",
        "Bash" => "💻",
        "Glob" => "🔍",
        "Grep" => "🔎",
        "Agent" => "🤖",
        "TaskCreate" | "TaskUpdate" => "📋",
        "ToolSearch" => "🔧",
        "Skill" => "⚡",
        _ => "  ",
    }
}

fn tool_color(name: &str) -> Color {
    match name {
        "Write" | "Edit" => Color::Green,
        "Read" | "Glob" | "Grep" => Color::Cyan,
        "Bash" => Color::Yellow,
        "Agent" => Color::Magenta,
        _ => Color::DarkGray,
    }
}

/// Compute the top N most-used tool names from a slice of turns.
fn top_tools(turns: &[Turn], n: usize) -> Vec<String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in turns {
        for tc in &t.tool_calls {
            *counts.entry(&tc.name).or_insert(0) += 1;
        }
    }
    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.into_iter().take(n).map(|(name, _)| name.to_string()).collect()
}

fn format_tokens(input: u64, output: u64) -> String {
    let fmt = |n: u64| -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            format!("{n}")
        }
    };
    format!("{}↓ {}↑", fmt(input), fmt(output))
}

fn time_from_iso(ts: &Option<String>) -> String {
    ts.as_ref()
        .and_then(|s| s.get(11..16))
        .unwrap_or("?")
        .to_string()
}

fn date_from_iso(ts: &Option<String>) -> String {
    ts.as_ref()
        .and_then(|s| s.get(..10))
        .unwrap_or("?")
        .to_string()
}

fn get_claude_version() -> String {
    std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Try to get the GitHub repo URL from a project directory.
fn get_git_remote_url(project_dir: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?.trim().to_string();
    // Convert SSH URLs to HTTPS for browser
    if url.starts_with("git@github.com:") {
        let path = url.strip_prefix("git@github.com:")?.strip_suffix(".git").unwrap_or(url.strip_prefix("git@github.com:")?);
        Some(format!("https://github.com/{path}"))
    } else if url.starts_with("https://") {
        Some(url.strip_suffix(".git").unwrap_or(&url).to_string())
    } else {
        Some(url)
    }
}

/// Convert epoch seconds to "YYYY-MM-DD" string (no chrono dependency).
fn epoch_to_date_str(epoch_secs: i64) -> String {
    // Civil date from Unix epoch using a simple algorithm
    let days = epoch_secs / 86400;
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Get the day-of-week (0=Mon, 6=Sun) from epoch seconds.
fn epoch_to_weekday(epoch_secs: i64) -> usize {
    // Jan 1 1970 was Thursday (3)
    let days = epoch_secs / 86400;
    ((days % 7 + 3) % 7) as usize // 0=Mon
}

fn relative_time(st: std::time::SystemTime) -> String {
    let secs = st.elapsed().unwrap_or_default().as_secs();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

// ── Build timeline items (shared between split preview and full screen) ─────

fn build_timeline_items(turns: &[Turn], expanded: &[bool], selected: Option<usize>, visible: Option<&[usize]>) -> Vec<ListItem<'static>> {
    let indices: Vec<usize> = match visible {
        Some(v) => v.to_vec(),
        None => (0..turns.len()).collect(),
    };
    let mut items = Vec::new();
    for (list_i, &turn_i) in indices.iter().enumerate() {
        let turn = &turns[turn_i];
        let i = list_i; // position in the displayed list
        let is_expanded = expanded.get(turn_i).copied().unwrap_or(false);
        let time = time_from_iso(&turn.timestamp);
        let arrow = if is_expanded { "▼" } else { "▶" };

        // Truncate prompt to fit — leave room for index + time prefix
        let prompt = if turn.prompt_text.len() > 50 {
            format!("{}…", &turn.prompt_text[..49])
        } else {
            turn.prompt_text.clone()
        };

        let style = if selected == Some(i) {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
            Span::styled(format!(" {arrow} "), Style::default().fg(Color::Blue)),
            Span::styled(
                format!("{:>2} ", turn.index),
                Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{time}  "), Style::default().fg(Color::DarkGray)),
            Span::styled(prompt, style),
        ])];

        // Always show tool calls + tokens on the second line (more info-dense)
        let tc = turn.tool_calls.len();
        let tk = format_tokens(turn.total_input_tokens, turn.total_output_tokens);
        if tc > 0 {
            lines.push(Line::from(vec![Span::styled(
                format!("          {tc} tool calls · {tk}"),
                Style::default().fg(Color::DarkGray),
            )]));
        }

        if is_expanded {
            for (j, tool) in turn.tool_calls.iter().enumerate() {
                let conn = if j == tc - 1 { "└─" } else { "├─" };
                let icon = tool_icon(&tool.name);
                let color = tool_color(&tool.name);
                let summary = if tool.summary.len() > 45 {
                    format!("{}…", &tool.summary[..44])
                } else {
                    tool.summary.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("       {conn} "), Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("{icon} ")),
                    Span::styled(format!("{} ", tool.name), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(summary, Style::default().fg(Color::DarkGray)),
                ]));
            }
            let fs = if turn.files_touched.is_empty() {
                String::new()
            } else {
                format!("  · {} file{}", turn.files_touched.len(), if turn.files_touched.len() == 1 { "" } else { "s" })
            };
            if !fs.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    format!("         {fs}"),
                    Style::default().fg(Color::DarkGray),
                )]));
            }
        }
        lines.push(Line::from(""));
        items.push(ListItem::new(lines));
    }
    items
}

// ── Render: Pin bar (shown at top when pins exist) ──────────────────────────

fn pin_bar_height(app: &App) -> u16 {
    if app.pins.is_empty() && !app.danger_mode { 0 } else { 1 }
}

fn render_pin_bar(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    if area.height == 0 {
        return;
    }
    let mut spans: Vec<Span> = Vec::new();

    if !app.pins.is_empty() {
        spans.push(Span::styled(" ★ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        for (i, pin) in app.pins.iter().enumerate() {
            let num = i + 1;
            spans.push(Span::styled(
                format!("[{num}]"),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {} ", pin.label),
                Style::default().fg(Color::Rgb(180, 180, 180)),
            ));
            if i < app.pins.len() - 1 {
                spans.push(Span::styled("· ", Style::default().fg(Color::DarkGray)));
            }
        }
    }

    if app.danger_mode {
        if !spans.is_empty() {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        } else {
            spans.push(Span::styled(" ", Style::default()));
        }
        spans.push(Span::styled(
            "⚠ DANGER MODE",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::Rgb(25, 20, 20)));
    frame.render_widget(bar, area);
}

// ── Render: Projects ────────────────────────────────────────────────────────

fn render_projects(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    let pbh = pin_bar_height(app);
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Length(pbh), Constraint::Min(5), Constraint::Length(3)])
        .split(area);
    render_pin_bar(frame, app, outer[1]);

    // Header — big block title
    let total_sessions: usize = app.projects.iter().map(|p| p.session_count).sum();
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("  ▄▀▀ █▀█ █▄ █ ▀█▀ █ █▄ █ █ █ █ █ █▄▀▄█", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]),
        Line::from({
            let mut spans = vec![
                Span::styled("  ▀▄▄ █▄█ █ ▀█  █  █ █ ▀█ █▄█ █▄█ █ ▀ █", Style::default().fg(Color::Cyan)),
                Span::styled(format!("   {} projects · {} sessions", app.projects.len(), total_sessions), Style::default().fg(Color::DarkGray)),
            ];
            if !app.agents.is_empty() {
                let running = app.agents.iter().filter(|a| !a.term.exited).count();
                spans.push(Span::styled(format!(" │ ●{running} running"), Style::default().fg(Color::Green)));
            }
            spans
        }),
    ])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, outer[0]);

    // Split: left (projects list 50%) | right (info panel 50%)
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[2]);

    // ── Left pane: project cards ──
    // Determine which projects to show (filtered or all)
    let visible_indices: Vec<usize> = if app.searching || !app.search_query.is_empty() {
        app.filtered_indices.clone()
    } else {
        (0..app.projects.len()).collect()
    };

    // Split left pane: list area + optional search bar
    let left_chunks = if app.searching {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(panes[0])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(0)])
            .split(panes[0])
    };

    let selected_idx = app.projects_state.selected();

    let card_w = 44usize; // inner width of card content area
    let border_h = "─".repeat(card_w);

    let items: Vec<ListItem> = visible_indices.iter().enumerate().map(|(list_i, &proj_i)| {
        let p = &app.projects[proj_i];
        let age = relative_time(p.latest_modified);
        let sl = if p.session_count == 1 { "1 session".to_string() } else { format!("{} sessions", p.session_count) };
        let is_selected = selected_idx == Some(list_i);

        // Colors: selected = bright/cyan, unselected = dim
        let border_color = if is_selected { Color::Cyan } else { Color::Rgb(55, 60, 70) };
        let shadow_color = if is_selected { Color::Rgb(20, 60, 80) } else { Color::Rgb(25, 28, 32) };
        let title_color = if is_selected { Color::White } else { Color::Rgb(160, 170, 180) };
        let path_color = if is_selected { Color::Rgb(100, 160, 180) } else { Color::DarkGray };
        let meta_color = if is_selected { Color::Rgb(130, 140, 150) } else { Color::Rgb(80, 90, 100) };

        // Pad a string to card_w, truncating if needed
        let pad = |s: &str| -> String {
            if s.len() >= card_w { s[..card_w].to_string() }
            else { format!("{s}{}", " ".repeat(card_w - s.len())) }
        };

        let title_str = format!("  {}", p.folder_name);
        let path_str = format!("  {}", p.display_name);
        let meta_str = format!("  {sl} · {age}");

        // Helper to make a content line: │ text │▐
        let card_line = |text: &str, style: Style| -> Line<'static> {
            Line::from(vec![
                Span::styled(" │", Style::default().fg(border_color)),
                Span::styled(pad(text), style),
                Span::styled("│", Style::default().fg(border_color)),
                Span::styled("▐", Style::default().fg(shadow_color)),
            ])
        };

        let mut lines = vec![
            // Top border
            Line::from(Span::styled(format!(" ╭{border_h}╮"), Style::default().fg(border_color))),
            card_line(&title_str, Style::default().fg(title_color).add_modifier(Modifier::BOLD)),
            card_line(&path_str, Style::default().fg(path_color)),
            card_line(&meta_str, Style::default().fg(meta_color)),
        ];

        // Expanded details (Tab toggle, only on selected card)
        if is_selected && app.project_expanded {
            let detail_s = Style::default().fg(Color::DarkGray);
            let val_s = Style::default().fg(Color::White);
            let sep = format!("  {}", "─".repeat(card_w - 4));
            lines.push(card_line(&sep, Style::default().fg(Color::Rgb(45, 50, 60))));
            lines.push(card_line(
                &format!("  Prompts: {}  Tools: {}", p.total_prompts, p.total_tools),
                val_s,
            ));
            let path_display = p.path.to_string_lossy();
            let dir_short = if path_display.len() > card_w - 4 {
                format!("  …{}", &path_display[path_display.len() - (card_w - 6)..])
            } else {
                format!("  {path_display}")
            };
            lines.push(card_line(&dir_short, detail_s));
        }

        // Bottom border + shadow
        lines.push(Line::from(vec![
            Span::styled(format!(" ╰{border_h}╯"), Style::default().fg(border_color)),
            Span::styled("▐", Style::default().fg(shadow_color)),
        ]));
        // Bottom shadow
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("▀".repeat(card_w + 2), Style::default().fg(shadow_color)),
        ]));
        ListItem::new(lines)
    }).collect();

    let list_title = if app.searching || !app.search_query.is_empty() {
        format!(" Projects ({}/{}) ", visible_indices.len(), app.projects.len())
    } else {
        format!(" Projects ({}) ", app.projects.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(list_title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        )
        .highlight_style(Style::default())
        .highlight_symbol("  ");
    frame.render_stateful_widget(list, left_chunks[0], &mut app.projects_state);

    // Search bar (when active)
    if app.searching {
        let search_text = format!(" /{}", app.search_query);
        let search_bar = Paragraph::new(Line::from(vec![
            Span::styled(&search_text, Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Cyan)), // cursor
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(" Search ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        );
        frame.render_widget(search_bar, left_chunks[1]);
    }

    // ── Right pane: split into top (logo+info) and bottom (stats) ──
    let right_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(panes[1]);

    // ── Top right: Calendar month heatmap ──
    let mut cal_lines: Vec<Line> = Vec::new();

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Get today's date components
    let today_str = epoch_to_date_str(now_secs);
    let today_month: u32 = today_str.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(1);
    let today_year: i64 = today_str.get(0..4).and_then(|s| s.parse().ok()).unwrap_or(2026);
    let today_day: u32 = today_str.get(8..10).and_then(|s| s.parse().ok()).unwrap_or(1);

    let month_names = ["", "January", "February", "March", "April", "May", "June",
                        "July", "August", "September", "October", "November", "December"];
    let month_name = month_names.get(today_month as usize).unwrap_or(&"?");

    // Find epoch for the 1st of this month
    let first_of_month_str = format!("{:04}-{:02}-01", today_year, today_month);
    // Walk back from today to find the 1st
    let mut first_epoch = now_secs - (today_day as i64 - 1) * 86400;
    // Verify it matches (might be off by timezone, but close enough for display)
    let first_date = epoch_to_date_str(first_epoch);
    let first_dow = epoch_to_weekday(first_epoch); // 0=Mon

    // Days in this month
    let days_in_month = match today_month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if today_year % 4 == 0 && (today_year % 100 != 0 || today_year % 400 == 0) { 29 } else { 28 },
        _ => 30,
    };

    // Collect activity for this month
    let mut month_counts: Vec<usize> = Vec::new(); // index 0 = day 1
    for d in 0..days_in_month {
        let day_epoch = first_epoch + d as i64 * 86400;
        let date = epoch_to_date_str(day_epoch);
        let count = app.activity_map.get(&date).copied().unwrap_or(0);
        month_counts.push(count);
    }
    let max_activity = month_counts.iter().copied().max().unwrap_or(1).max(1);

    // #D4A574 = (212, 165, 116)
    let heat_color = |count: usize| -> Color {
        if count == 0 {
            Color::Rgb(45, 42, 40)
        } else {
            let ratio = (count as f64 / max_activity as f64).min(1.0);
            if ratio < 0.33 {
                let t = ratio * 3.0;
                let r = (90.0 + t * 50.0) as u8;
                let g = (75.0 + t * 40.0) as u8;
                let b = (60.0 + t * 26.0) as u8;
                Color::Rgb(r, g, b)
            } else if ratio < 0.66 {
                let t = (ratio - 0.33) * 3.0;
                let r = (140.0 + t * 72.0) as u8;
                let g = (115.0 + t * 50.0) as u8;
                let b = (86.0 + t * 30.0) as u8;
                Color::Rgb(r, g, b)
            } else {
                let t = (ratio - 0.66) * 3.0;
                let r = (212.0 + t.min(1.0) * 28.0) as u8;
                let g = (165.0 + t.min(1.0) * 30.0) as u8;
                let b = (116.0 + t.min(1.0) * 24.0) as u8;
                Color::Rgb(r, g, b)
            }
        }
    };

    // Month + year header
    cal_lines.push(Line::from(""));
    cal_lines.push(Line::from(vec![
        Span::styled(
            format!("     {month_name} {today_year}"),
            Style::default().fg(Color::Rgb(212, 165, 116)).add_modifier(Modifier::BOLD),
        ),
    ]));
    cal_lines.push(Line::from(""));

    // Day-of-week header
    cal_lines.push(Line::from(vec![
        Span::styled("   Mon  Tue  Wed  Thu  Fri  Sat  Sun", Style::default().fg(Color::DarkGray)),
    ]));

    // Calendar grid — each row is a week
    let mut day = 1u32;
    let mut week_row = 0;
    loop {
        if day > days_in_month as u32 {
            break;
        }
        let mut spans: Vec<Span> = vec![Span::styled("   ", Style::default())];

        for dow in 0..7usize {
            if (week_row == 0 && dow < first_dow) || day > days_in_month as u32 {
                // Empty cell (before month starts or after it ends)
                spans.push(Span::styled("     ", Style::default()));
            } else {
                let count = month_counts.get((day - 1) as usize).copied().unwrap_or(0);
                let is_today = day == today_day;
                let is_future = day > today_day;

                if is_future {
                    // Future days — dim
                    spans.push(Span::styled(
                        format!(" {:>2}  ", day),
                        Style::default().fg(Color::Rgb(55, 53, 50)),
                    ));
                } else if count > 0 {
                    // Active day — show ✱ with heat color
                    let color = heat_color(count);
                    if is_today {
                        spans.push(Span::styled(
                            format!("[{:>2}] ", day),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::styled(
                            format!(" {:>2}✱ ", day),
                            Style::default().fg(color),
                        ));
                    }
                } else {
                    // No activity
                    if is_today {
                        spans.push(Span::styled(
                            format!("[{:>2}] ", day),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::styled(
                            format!(" {:>2}  ", day),
                            Style::default().fg(Color::Rgb(80, 78, 75)),
                        ));
                    }
                }
                day += 1;
            }
        }
        cal_lines.push(Line::from(spans));
        cal_lines.push(Line::from("")); // spacing between weeks
        week_row += 1;
    }

    // Summary stats
    let total_month: usize = month_counts.iter().sum();
    let active_days = month_counts.iter().filter(|c| **c > 0).count();
    cal_lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(format!("{total_month}"), Style::default().fg(Color::Rgb(212, 165, 116)).add_modifier(Modifier::BOLD)),
        Span::styled(" sessions  ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{active_days}"), Style::default().fg(Color::Rgb(212, 165, 116)).add_modifier(Modifier::BOLD)),
        Span::styled(format!("/{today_day} active days"), Style::default().fg(Color::DarkGray)),
    ]));

    // Legend
    cal_lines.push(Line::from(vec![
        Span::styled("   Less ", Style::default().fg(Color::DarkGray)),
        Span::styled("✱ ", Style::default().fg(Color::Rgb(45, 42, 40))),
        Span::styled("✱ ", Style::default().fg(Color::Rgb(140, 115, 86))),
        Span::styled("✱ ", Style::default().fg(Color::Rgb(212, 165, 116))),
        Span::styled("✱ ", Style::default().fg(Color::Rgb(240, 195, 140))),
        Span::styled(" More", Style::default().fg(Color::DarkGray)),
        Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&app.claude_version, Style::default().fg(Color::Rgb(120, 118, 115))),
    ]));

    let cal_para = Paragraph::new(cal_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(" Activity ", Style::default().fg(Color::Rgb(212, 165, 116)).add_modifier(Modifier::BOLD))),
        );
    frame.render_widget(cal_para, right_split[0]);

    // ── Bottom right: aggregate stats dashboard ──
    let total_sessions: usize = app.projects.iter().map(|p| p.session_count).sum();
    let total_prompts: usize = app.projects.iter().map(|p| p.total_prompts).sum();
    let total_tools: usize = app.projects.iter().map(|p| p.total_tools).sum();

    // Top active projects (by session count)
    let mut by_sessions: Vec<(&ProjectEntry, usize)> = app.projects.iter().map(|p| (p, p.session_count)).collect();
    by_sessions.sort_by(|a, b| b.1.cmp(&a.1));

    let label_s = Style::default().fg(Color::DarkGray);
    let value_s = Style::default().fg(Color::White);
    let accent_s = Style::default().fg(Color::Cyan);

    let mut stat_lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Total Sessions:   ", label_s),
            Span::styled(format!("{total_sessions}"), value_s),
        ]),
        Line::from(vec![
            Span::styled("  Total Prompts:    ", label_s),
            Span::styled(format!("{total_prompts}"), value_s),
        ]),
        Line::from(vec![
            Span::styled("  Total Tool Calls: ", label_s),
            Span::styled(format!("{total_tools}"), value_s),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Most Active Projects", accent_s.add_modifier(Modifier::BOLD))),
    ];

    for (p, count) in by_sessions.iter().take(5) {
        let bar_len = (*count as f64 / by_sessions[0].1.max(1) as f64 * 12.0) as usize;
        let bar = "█".repeat(bar_len.max(1));
        stat_lines.push(Line::from(vec![
            Span::styled("    ", label_s),
            Span::styled(p.folder_name.clone(), Style::default().fg(Color::White)),
        ]));
        stat_lines.push(Line::from(vec![
            Span::styled("      ", label_s),
            Span::styled(bar, accent_s),
            Span::styled(format!(" {count}"), Style::default().fg(Color::DarkGray)),
        ]));
    }

    let stats_para = Paragraph::new(stat_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(" Dashboard ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        );
    frame.render_widget(stats_para, right_split[1]);

    // Footer
    let mut footer_spans = vec![
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Tab", Style::default().fg(Color::Cyan)),
        Span::styled(" details  ", Style::default().fg(Color::DarkGray)),
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::styled(" search  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Shift+D", Style::default().fg(Color::Red)),
        Span::styled(" danger  ", Style::default().fg(Color::DarkGray)),
    ];
    if !app.agents.is_empty() {
        footer_spans.extend(vec![
            Span::styled("C-Space", Style::default().fg(Color::Yellow)),
            Span::styled(" agents  ", Style::default().fg(Color::DarkGray)),
        ]);
    }
    if !app.pins.is_empty() {
        footer_spans.extend(vec![
            Span::styled("1-9", Style::default().fg(Color::Yellow)),
            Span::styled(" jump pin  ", Style::default().fg(Color::DarkGray)),
        ]);
    }
    footer_spans.extend(vec![
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]);

    let footer = Paragraph::new(Line::from(footer_spans))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, outer[3]);
}

// ── Render: Session Split ───────────────────────────────────────────────────

fn render_session_split(frame: &mut ratatui::Frame, app: &mut App, project_idx: usize, focus: Pane) {
    let area = frame.area();
    let pbh = pin_bar_height(app);
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(pbh), Constraint::Min(5), Constraint::Length(3)])
        .split(area);
    render_pin_bar(frame, app, outer[1]);

    let project_path = if project_idx < app.projects.len() {
        app.projects[project_idx].display_name.clone()
    } else {
        "?".to_string()
    };

    // Header — project path + session count + agent badge
    let mut header_spans = vec![
        Span::styled(" Continuum ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&project_path, Style::default().fg(Color::Rgb(100, 150, 180))),
        Span::styled(format!(" │ {} sessions", app.sessions.len()), Style::default().fg(Color::DarkGray)),
    ];
    if !app.agents.is_empty() {
        let running = app.agents.iter().filter(|a| !a.term.exited).count();
        header_spans.push(Span::styled(format!(" │ ●{running} running"), Style::default().fg(Color::Green)));
    }
    let header = Paragraph::new(Line::from(header_spans))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, outer[0]);

    // 3-pane layout: sessions (30%) | timeline (45%) | details (25%)
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(45), Constraint::Percentage(25)])
        .split(outer[2]);

    // ── Left pane: sessions list ──
    let left_border_color = if focus == Pane::Left { Color::Cyan } else { Color::DarkGray };

    let session_items: Vec<ListItem> = app.sessions.iter().enumerate().map(|(i, s)| {
        let head_label = if i == 0 { "  HEAD" } else { "" };
        let marker = if i == 0 { "●" } else { "○" };
        let mc = if i == 0 { Color::Green } else { Color::DarkGray };
        let branch = s.info.git_branch.as_deref().unwrap_or("");
        let branch_display = if branch.is_empty() { String::new() } else { format!("  {branch}") };
        let date = date_from_iso(&s.info.started_at);
        let time = s.info.started_at.as_ref().and_then(|t| t.get(11..16)).unwrap_or("?");

        ListItem::new(vec![
            Line::from(vec![
                Span::styled(format!("  {marker} "), Style::default().fg(mc)),
                Span::styled(&s.session_id, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(head_label, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled(branch_display, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(vec![Span::styled(
                format!("      {} prompts · {} tools", s.info.prompt_count, s.info.tool_call_count),
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(vec![Span::styled(
                format!("      {date} {time}"),
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(""),
        ])
    }).collect();

    let session_list = List::new(session_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(left_border_color))
                .title(Span::styled(
                    " Sessions ",
                    Style::default().fg(left_border_color).add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(Style::default().bg(Color::Rgb(20, 50, 60)))
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(session_list, panes[0], &mut app.sessions_state);

    // ── Middle pane: timeline preview ──
    let mid_border_color = if focus == Pane::Right { Color::Cyan } else { Color::DarkGray };

    let mid_title = if let Some(ref info) = app.info {
        let branch = info.git_branch.as_deref().unwrap_or("—");
        let tk = format_tokens(info.total_input_tokens, info.total_output_tokens);
        format!(" Timeline ({branch}) · {tk} ")
    } else {
        " Timeline ".to_string()
    };

    // Split middle pane for optional search bar
    let mid_chunks = if app.timeline_searching {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(panes[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(0)])
            .split(panes[1])
    };

    let mid_title = if app.timeline_searching || !app.timeline_search_query.is_empty() {
        let vis_count = app.timeline_filtered_indices.len();
        let total = app.turns.len();
        format!(" Timeline ({vis_count}/{total}) · {}", mid_title.trim())
    } else {
        mid_title
    };

    if app.turns.is_empty() {
        let empty_msg = if app.last_error.is_some() {
            "Error loading session"
        } else {
            "Select a session"
        };
        let empty = Paragraph::new(Line::from(vec![
            Span::styled(empty_msg, Style::default().fg(Color::DarkGray)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(mid_border_color))
                .title(Span::styled(&mid_title, Style::default().fg(mid_border_color).add_modifier(Modifier::BOLD))),
        );
        frame.render_widget(empty, mid_chunks[0]);
    } else {
        let sel = if focus == Pane::Right { app.timeline_state.selected() } else { None };
        let vis = if app.timeline_searching || !app.timeline_search_query.is_empty() {
            Some(app.timeline_filtered_indices.as_slice())
        } else { None };
        let items = build_timeline_items(&app.turns, &app.expanded, sel, vis);
        let timeline_list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(mid_border_color))
                    .title(Span::styled(&mid_title, Style::default().fg(mid_border_color).add_modifier(Modifier::BOLD))),
            )
            .highlight_style(if focus == Pane::Right {
                Style::default().bg(Color::Rgb(20, 50, 60))
            } else {
                Style::default()
            });
        frame.render_stateful_widget(timeline_list, mid_chunks[0], &mut app.timeline_state);
    }

    // Timeline search bar
    if app.timeline_searching {
        let search_text = format!(" /{}", app.timeline_search_query);
        let search_bar = Paragraph::new(Line::from(vec![
            Span::styled(&search_text, Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(" Search Timeline ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        );
        frame.render_widget(search_bar, mid_chunks[1]);
    }

    // ── Right pane: details panel ──
    render_details_panel(frame, app, panes[2]);

    // Footer
    let focus_hint = if focus == Pane::Left {
        vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan)),
            Span::styled("/", Style::default().fg(Color::DarkGray)),
            Span::styled("→", Style::default().fg(Color::Cyan)),
            Span::styled(" timeline  ", Style::default().fg(Color::DarkGray)),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::styled(" star  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Shift+F", Style::default().fg(Color::Yellow)),
            Span::styled(" fork  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Shift+D", Style::default().fg(Color::Red)),
            Span::styled(" danger  ", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan)),
            Span::styled("/", Style::default().fg(Color::DarkGray)),
            Span::styled("←", Style::default().fg(Color::Cyan)),
            Span::styled(" sessions  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::styled(" expand  ", Style::default().fg(Color::DarkGray)),
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::styled(" search  ", Style::default().fg(Color::DarkGray)),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::styled(" star  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Shift+F", Style::default().fg(Color::Yellow)),
            Span::styled(" fork  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Shift+D", Style::default().fg(Color::Red)),
            Span::styled(" danger  ", Style::default().fg(Color::DarkGray)),
        ]
    };

    let mut footer_spans = vec![
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
    ];
    footer_spans.extend(focus_hint);
    if !app.agents.is_empty() {
        footer_spans.extend(vec![
            Span::styled("C-Space", Style::default().fg(Color::Yellow)),
            Span::styled(" agents  ", Style::default().fg(Color::DarkGray)),
        ]);
    }
    if !app.pins.is_empty() {
        footer_spans.extend(vec![
            Span::styled("1-9", Style::default().fg(Color::Yellow)),
            Span::styled(" jump pin  ", Style::default().fg(Color::DarkGray)),
        ]);
    }
    footer_spans.extend(vec![
        Span::styled("o", Style::default().fg(Color::Cyan)),
        Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]);

    let footer = Paragraph::new(Line::from(footer_spans))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, outer[3]);
}

// ── Render: Details panel ────────────────────────────────────────────────────

fn render_details_panel(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let info = match app.info.as_ref() {
        Some(i) => i,
        None => {
            let empty = Paragraph::new("")
                .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(" Details ", Style::default().fg(Color::DarkGray))));
            frame.render_widget(empty, area);
            return;
        }
    };

    let sid_short = if info.session_id.len() > 8 {
        &info.session_id[..8]
    } else {
        &info.session_id
    };
    let head_label = if app.sessions_state.selected() == Some(0) { " HEAD" } else { "" };
    let title = format!(" Details ({sid_short}{head_label}) ");

    let tk = format_tokens(info.total_input_tokens, info.total_output_tokens);
    let started = info.started_at.as_ref().and_then(|s| s.get(..16)).unwrap_or("—");
    let last_activity = app.turns.last()
        .and_then(|t| t.timestamp.as_ref())
        .and_then(|s| s.get(..16))
        .unwrap_or("—");

    let label_style = Style::default().fg(Color::DarkGray);
    let value_style = Style::default().fg(Color::White);

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Total Prompts:  ", label_style),
            Span::styled(format!("{}", info.prompt_count), value_style),
        ]),
        Line::from(vec![
            Span::styled("  Total Tools:    ", label_style),
            Span::styled(format!("{}", info.tool_call_count), value_style),
        ]),
        Line::from(vec![
            Span::styled("  Data Transfer:  ", label_style),
            Span::styled(&tk, value_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Active Since:   ", label_style),
            Span::styled(started, value_style),
        ]),
        Line::from(vec![
            Span::styled("  Last Activity:  ", label_style),
            Span::styled(last_activity, value_style),
        ]),
        Line::from(""),
    ];

    // Git branch
    let branch = info.git_branch.as_deref().unwrap_or("—");
    lines.push(Line::from(vec![
        Span::styled("  Branch:         ", label_style),
        Span::styled(branch, Style::default().fg(Color::Cyan)),
    ]));

    // Unique files modified (deduplicated across all turns)
    let mut unique_files: Vec<&str> = Vec::new();
    for t in &app.turns {
        for f in &t.files_touched {
            if !unique_files.contains(&f.as_str()) {
                unique_files.push(f.as_str());
            }
        }
    }
    lines.push(Line::from(vec![
        Span::styled("  Files Modified: ", label_style),
        Span::styled(format!("{}", unique_files.len()), value_style),
    ]));

    // Session duration (first turn → last turn)
    let duration_str = match (
        app.turns.first().and_then(|t| t.timestamp.as_ref()),
        app.turns.last().and_then(|t| t.timestamp.as_ref()),
    ) {
        (Some(start), Some(end)) => {
            // Parse ISO timestamps and compute difference
            let parse_secs = |ts: &str| -> Option<i64> {
                // Extract HH:MM from "YYYY-MM-DDTHH:MM:SS"
                let date_part = ts.get(..10)?;
                let time_part = ts.get(11..19)?;
                let parts: Vec<&str> = date_part.split('-').collect();
                if parts.len() != 3 { return None; }
                let day: i64 = parts[2].parse().ok()?;
                let hour: i64 = time_part.get(..2)?.parse().ok()?;
                let min: i64 = time_part.get(3..5)?.parse().ok()?;
                let sec: i64 = time_part.get(6..8)?.parse().ok()?;
                Some(day * 86400 + hour * 3600 + min * 60 + sec)
            };
            match (parse_secs(start), parse_secs(end)) {
                (Some(s), Some(e)) if e >= s => {
                    let diff = e - s;
                    if diff < 60 { format!("{diff}s") }
                    else if diff < 3600 { format!("{}m {}s", diff / 60, diff % 60) }
                    else { format!("{}h {}m", diff / 3600, (diff % 3600) / 60) }
                }
                _ => "—".to_string(),
            }
        }
        _ => "—".to_string(),
    };
    lines.push(Line::from(vec![
        Span::styled("  Duration:       ", label_style),
        Span::styled(duration_str, value_style),
    ]));

    // Heaviest turn (most tool calls)
    if let Some(heaviest) = app.turns.iter().max_by_key(|t| t.tool_calls.len()) {
        if !heaviest.tool_calls.is_empty() {
            let prompt_short = if heaviest.prompt_text.len() > 18 {
                format!("{}…", &heaviest.prompt_text[..17])
            } else {
                heaviest.prompt_text.clone()
            };
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  Heaviest Turn", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled(format!("    #{} ", heaviest.index), Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                Span::styled(prompt_short, Style::default().fg(Color::Rgb(160, 170, 180))),
            ]));
            lines.push(Line::from(vec![
                Span::styled(format!("    {} tools · {}", heaviest.tool_calls.len(), format_tokens(heaviest.total_input_tokens, heaviest.total_output_tokens)), label_style),
            ]));
        }
    }

    // GitHub repo link
    if let Some(url) = get_git_remote_url(&info.project_dir) {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Repo:", label_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    ", label_style),
            Span::styled(url, Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED)),
        ]));
    }

    let details = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        );
    frame.render_widget(details, area);
}

// ── Render: Full Timeline ───────────────────────────────────────────────────

fn render_timeline(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    let pbh = pin_bar_height(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(pbh), Constraint::Min(5), Constraint::Length(3)])
        .split(area);
    render_pin_bar(frame, app, chunks[1]);

    let info = app.info.as_ref();
    let pn = info.map(|i| i.project_dir.split('/').last().unwrap_or(&i.project_dir)).unwrap_or("?");
    let br = info.and_then(|i| i.git_branch.as_deref()).unwrap_or("—");
    let tk = info.map(|i| format_tokens(i.total_input_tokens, i.total_output_tokens)).unwrap_or_default();

    let mut hdr_spans = vec![
        Span::styled(" Continuum ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(pn, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" ({br}) "), Style::default().fg(Color::DarkGray)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&tk, Style::default().fg(Color::DarkGray)),
    ];
    if !app.agents.is_empty() {
        let running = app.agents.iter().filter(|a| !a.term.exited).count();
        hdr_spans.push(Span::styled(format!(" │ ●{running} running"), Style::default().fg(Color::Green)));
    }
    let header = Paragraph::new(Line::from(hdr_spans))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, chunks[0]);

    let vis_tl = if app.timeline_searching || !app.timeline_search_query.is_empty() {
        Some(app.timeline_filtered_indices.as_slice())
    } else { None };
    let items = build_timeline_items(&app.turns, &app.expanded, app.timeline_state.selected(), vis_tl);
    let list = List::new(items)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT).border_style(Style::default().fg(Color::DarkGray)).padding(Padding::new(0, 0, 1, 0)))
        .highlight_style(Style::default().bg(Color::Rgb(20, 50, 60)));
    frame.render_stateful_widget(list, chunks[2], &mut app.timeline_state);

    let tc = app.turns.len();
    let sel = app.timeline_state.selected().map(|s| s + 1).unwrap_or(0);
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(format!(" {sel}/{tc} "), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::styled(" expand  ", Style::default().fg(Color::DarkGray)),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::styled(" star  ", Style::default().fg(Color::DarkGray)),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Shift+F", Style::default().fg(Color::Yellow)),
        Span::styled(" fork  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Shift+D", Style::default().fg(Color::Red)),
        Span::styled(" danger  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, chunks[3]);
}

// ── Render: Embedded Claude Code ─────────────────────────────────────────────

fn render_embedded(frame: &mut ratatui::Frame, app: &mut App, focus: Pane) {
    let area = frame.area();
    let pbh = pin_bar_height(app);
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(pbh), Constraint::Min(5), Constraint::Length(1)])
        .split(area);
    render_pin_bar(frame, app, outer[1]);

    // Determine active agent status
    let active_agent = app.active_agent_idx.and_then(|i| app.agents.get_mut(i));
    let (agent_label, running) = match active_agent {
        Some(a) => (a.label.clone(), a.term.is_running()),
        None => ("—".to_string(), false),
    };
    let (status, status_color) = if running { ("RUNNING", Color::Green) } else { ("EXITED", Color::Red) };
    let agent_count = app.agents.len();

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Continuum ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&agent_label, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(status, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" │ {agent_count} agent{}", if agent_count == 1 { "" } else { "s" }), Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, outer[0]);

    // Split: left pane (carousel: timeline or agents list), right=terminal
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(outer[2]);

    if app.left_pane_mode == LeftPaneMode::Agents {
        // ── Left pane: agents list ──
        render_agents_list(frame, app, panes[0], focus == Pane::Left);
    } else {
        // ── Left pane: timeline ──
        let left_border_color = if focus == Pane::Left { Color::Cyan } else { Color::DarkGray };
        let sel = if focus == Pane::Left { app.timeline_state.selected() } else { None };
        let items = build_timeline_items(&app.turns, &app.expanded, sel, None);
        let timeline_list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(left_border_color))
                    .title(Span::styled(" Timeline ", Style::default().fg(left_border_color).add_modifier(Modifier::BOLD))),
            )
            .highlight_style(if focus == Pane::Left {
                Style::default().bg(Color::Rgb(20, 50, 60))
            } else {
                Style::default()
            });
        frame.render_stateful_widget(timeline_list, panes[0], &mut app.timeline_state);
    }

    // Right pane: embedded terminal
    let right_border_color = if focus == Pane::Right { Color::Cyan } else { Color::DarkGray };
    let right_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(right_border_color));

    let inner = right_block.inner(panes[1]);
    frame.render_widget(right_block, panes[1]);

    // Store the area for mouse coordinate mapping
    app.embedded_term_area = inner;

    if let Some(idx) = app.active_agent_idx {
        if let Some(agent) = app.agents.get(idx) {
            let mut lines = agent.term.render_lines(inner.height, inner.width);
            // Show scroll indicator when viewport is not at the bottom
            if agent.term.is_scrolled() {
                let offset = agent.term.viewport_offset;
                let indicator = format!(" ↑ SCROLLED +{offset} rows — type to snap back ");
                if let Some(last) = lines.last_mut() {
                    *last = Line::from(Span::styled(
                        indicator,
                        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ));
                }
            }
            let para = Paragraph::new(lines);
            frame.render_widget(para, inner);
        }
    }

    // Footer
    let mode_label = if app.left_pane_mode == LeftPaneMode::Agents { "Timeline" } else { "Agents" };
    let hint = if focus == Pane::Right {
        format!(" Esc Esc → left pane │ Ctrl+] also works │ Scroll to browse │ a → {mode_label} │ All keys → Claude ")
    } else if app.left_pane_mode == LeftPaneMode::Agents {
        format!(" ↑↓ nav │ Enter switch │ a → {mode_label} │ Tab/→ terminal │ x kill │ Esc close ")
    } else {
        format!(" ↑↓ nav │ Enter expand │ a → {mode_label} │ Tab/→ terminal │ Esc close │ q quit ")
    };
    let footer = Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, outer[3]);
}

// ── Render: Agents list (left pane carousel) ─────────────────────────────────

fn render_agents_list(frame: &mut ratatui::Frame, app: &mut App, area: ratatui::layout::Rect, focused: bool) {
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let active_idx = app.active_agent_idx;

    let items: Vec<ListItem> = app.agents.iter().enumerate().map(|(i, agent)| {
        let is_active = active_idx == Some(i);
        let running = !agent.term.exited;
        let status_icon = if running { "●" } else { "○" };
        let status_color = if running { Color::Green } else { Color::Red };
        let active_marker = if is_active { " ◀" } else { "" };

        let elapsed = agent.started_at.elapsed().as_secs();
        let uptime = if elapsed < 60 {
            format!("{elapsed}s")
        } else if elapsed < 3600 {
            format!("{}m", elapsed / 60)
        } else {
            format!("{}h", elapsed / 3600)
        };

        let kind = if agent.forked { "fork" } else { "resume" };

        ListItem::new(vec![
            Line::from(vec![
                Span::styled(format!("  {status_icon} "), Style::default().fg(status_color)),
                Span::styled(&agent.label, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(active_marker, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![Span::styled(
                format!("      {kind} · {uptime}"),
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(""),
        ])
    }).collect();

    let title = format!(" Agents ({}) ", app.agents.len());
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(title, Style::default().fg(border_color).add_modifier(Modifier::BOLD))),
        )
        .highlight_style(if focused {
            Style::default().bg(Color::Rgb(20, 50, 60))
        } else {
            Style::default()
        });
    frame.render_stateful_widget(list, area, &mut app.agents_state);
}

// ── Render: Agent Command Palette (Spotlight-style overlay) ──────────────────

fn render_agent_palette(frame: &mut ratatui::Frame, app: &mut App) {
    if !app.agent_palette_open || app.agents.is_empty() {
        return;
    }

    let full_area = frame.area();
    let popup_bg = Color::Rgb(15, 18, 22);
    let dim_bg = Color::Rgb(5, 6, 8);
    let highlight_bg = Color::Rgb(25, 45, 60);
    let border_style = Style::default().fg(Color::Cyan).bg(popup_bg);
    let active_idx = app.active_agent_idx;
    let selected = app.agent_palette_state.selected();

    // ── Layer 1: Full-screen dim backdrop ──
    // Paint EVERY cell in the frame to a dark space so NO background content
    // is visible anywhere — not inside the popup, not around it.
    {
        let buf = frame.buffer_mut();
        for cy in full_area.top()..full_area.bottom() {
            for cx in full_area.left()..full_area.right() {
                if let Some(cell) = buf.cell_mut((cx, cy)) {
                    cell.set_char(' ');
                    cell.fg = dim_bg;
                    cell.bg = dim_bg;
                    cell.modifier = Modifier::empty();
                }
            }
        }
    }

    // ── Layer 2: Popup with manually padded lines ──
    // Every line is EXACTLY popup_w display-columns wide so the Paragraph
    // widget writes to every cell in popup_area — zero unwritten cells.
    let popup_w = (full_area.width * 2 / 3).max(60).min(full_area.width.saturating_sub(4));
    let popup_h = (app.agents.len() as u16 + 5).min(full_area.height.saturating_sub(4)).max(7);
    let x0 = (full_area.width.saturating_sub(popup_w)) / 2;
    let y0 = (full_area.height.saturating_sub(popup_h)) / 2;
    let popup_area = ratatui::layout::Rect::new(x0, y0, popup_w, popup_h);
    let w = popup_w as usize;
    let inner_w = w.saturating_sub(2); // minus left and right │

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(popup_h as usize);

    // ── Top border: ╭─ Agents (N) ──...──╮ ──
    let title = format!(" Agents ({}) ", app.agents.len());
    let title_w = title.len(); // ASCII only
    let fill = inner_w.saturating_sub(1 + title_w);
    lines.push(Line::from(vec![
        Span::styled("╭─", border_style),
        Span::styled(title, Style::default().fg(Color::Cyan).bg(popup_bg).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill), border_style),
        Span::styled("╮", border_style),
    ]));

    // ── Content rows ──
    let content_rows = popup_h.saturating_sub(3) as usize;
    for i in 0..content_rows {
        if i < app.agents.len() {
            let agent = &app.agents[i];
            let is_active = active_idx == Some(i);
            let is_selected = selected == Some(i);
            let running = !agent.term.exited;
            let row_bg = if is_selected { highlight_bg } else { popup_bg };

            let dot = if running { "●" } else { "○" };
            let dot_color = if running { Color::Green } else { Color::Red };
            let marker_s: String = if is_selected { " ▸".into() } else { "  ".into() };
            let num = i + 1;
            let elapsed = agent.started_at.elapsed().as_secs();
            let uptime = if elapsed < 60 { format!("{elapsed}s") }
                else if elapsed < 3600 { format!("{}m", elapsed / 60) }
                else { format!("{}h", elapsed / 3600) };
            let state_label = if running { "running" } else { "done" };
            let state_color = if running { Color::DarkGray } else { Color::Red };
            let name_fg = if is_active { Color::Cyan } else { Color::White };
            let name_mod = if is_active { Modifier::BOLD } else { Modifier::empty() };
            let active_s: String = if is_active { "  ◀ active".into() } else { String::new() };

            let mut spans: Vec<Span<'static>> = vec![
                Span::styled("│", border_style),
                Span::styled(marker_s, Style::default().fg(Color::Cyan).bg(row_bg)),
                Span::styled(format!("{dot}{num} "), Style::default().fg(dot_color).bg(row_bg).add_modifier(Modifier::BOLD)),
                Span::styled(agent.label.clone(), Style::default().fg(name_fg).bg(row_bg).add_modifier(name_mod)),
                Span::styled(format!("   {uptime}   "), Style::default().fg(Color::DarkGray).bg(row_bg)),
                Span::styled(state_label.to_string(), Style::default().fg(state_color).bg(row_bg)),
            ];
            if is_active {
                spans.push(Span::styled(active_s, Style::default().fg(Color::Yellow).bg(row_bg)));
            }
            let content_w: usize = spans.iter().skip(1).map(|s| s.width()).sum();
            let pad = inner_w.saturating_sub(content_w);
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(row_bg)));
            spans.push(Span::styled("│", border_style));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::styled("│", border_style),
                Span::styled(" ".repeat(inner_w), Style::default().bg(popup_bg)),
                Span::styled("│", border_style),
            ]));
        }
    }

    // ── Bottom border: ╰──...──╯ ──
    lines.push(Line::from(vec![
        Span::styled("╰", border_style),
        Span::styled("─".repeat(w.saturating_sub(2)), border_style),
        Span::styled("╯", border_style),
    ]));

    // ── Footer hints ──
    let mut footer_spans: Vec<Span<'static>> = vec![
        Span::styled(" 1-9", Style::default().fg(Color::Cyan).bg(popup_bg)),
        Span::styled(" jump  ", Style::default().fg(Color::DarkGray).bg(popup_bg)),
        Span::styled("↑↓", Style::default().fg(Color::Cyan).bg(popup_bg)),
        Span::styled(" select  ", Style::default().fg(Color::DarkGray).bg(popup_bg)),
        Span::styled("Enter", Style::default().fg(Color::Cyan).bg(popup_bg)),
        Span::styled(" switch  ", Style::default().fg(Color::DarkGray).bg(popup_bg)),
        Span::styled("x", Style::default().fg(Color::Red).bg(popup_bg)),
        Span::styled(" kill  ", Style::default().fg(Color::DarkGray).bg(popup_bg)),
        Span::styled("Esc", Style::default().fg(Color::Cyan).bg(popup_bg)),
        Span::styled(" close", Style::default().fg(Color::DarkGray).bg(popup_bg)),
    ];
    let footer_w: usize = footer_spans.iter().map(|s| s.width()).sum();
    let footer_pad = w.saturating_sub(footer_w);
    footer_spans.push(Span::styled(" ".repeat(footer_pad), Style::default().bg(popup_bg)));
    lines.push(Line::from(footer_spans));

    // ── Render as single Paragraph — writes to every cell ──
    let popup = Paragraph::new(lines).style(Style::default().bg(popup_bg));
    frame.render_widget(popup, popup_area);
}

// ── Main render dispatch ────────────────────────────────────────────────────

fn ui(frame: &mut ratatui::Frame, app: &mut App) {
    match app.screen.clone() {
        Screen::Projects => render_projects(frame, app),
        Screen::SessionSplit { project_idx, focus } => render_session_split(frame, app, project_idx, focus),
        Screen::Timeline => render_timeline(frame, app),
        Screen::Embedded { focus } => render_embedded(frame, app, focus),
    }
    // Overlay the agent palette on top of any screen
    render_agent_palette(frame, app);
}

// ── Find transcript for CWD ─────────────────────────────────────────────────

fn find_latest_transcript() -> Option<PathBuf> {
    let base = dirs::home_dir()?.join(".claude/projects");
    let cwd = std::env::current_dir().ok()?;
    let cwd_key = cwd.to_string_lossy().replace('/', "-");
    let project_dir = base.join(&cwd_key);
    if !project_dir.exists() {
        return None;
    }
    let mut transcripts: Vec<PathBuf> = std::fs::read_dir(&project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl") && !p.to_string_lossy().contains("subagents"))
        .collect();
    transcripts.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });
    transcripts.into_iter().next()
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1);

    let mut app = match &arg {
        Some(path) if path != "--projects" && path != "-p" => {
            let p = PathBuf::from(path);
            if !p.exists() {
                return Err(format!("File not found: {}", p.display()).into());
            }
            App::new_with_transcript(p)
        }
        _ => {
            if arg.as_deref() == Some("--projects") || arg.as_deref() == Some("-p") {
                App::new()
            } else if let Some(path) = find_latest_transcript() {
                App::new_with_transcript(path)
            } else {
                App::new()
            }
        }
    };

    // File watcher for timeline auto-refresh
    let (fs_tx, fs_rx) = mpsc::channel();
    let mut _watcher: Option<Box<dyn Watcher>> = None;
    if let Some(ref path) = app.transcript_path {
        let tx = fs_tx.clone();
        let mut w = recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(ev) = res {
                if ev.kind.is_modify() {
                    let _ = tx.send(());
                }
            }
        })?;
        w.watch(path, RecursiveMode::NonRecursive)?;
        _watcher = Some(Box::new(w));
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Enable kitty keyboard protocol for proper Shift+Enter detection
    let kb_enhanced = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    loop {
        // Process output for ALL running agents
        for agent in &mut app.agents {
            agent.term.process_output();
        }

        terminal.draw(|f| ui(f, &mut app))?;

        if fs_rx.try_recv().is_ok() {
            while fs_rx.try_recv().is_ok() {}
            app.reload_timeline();
        }

        // Shorter poll when embedded terminal is active (smoother rendering)
        let poll_ms = if !app.agents.is_empty() { 16 } else { 100 };
        if event::poll(Duration::from_millis(poll_ms))? {
            let ev = event::read()?;

            // Mouse scroll in embedded terminal → move viewport through oversized PTY
            if let Event::Mouse(mouse) = &ev {
                if let Screen::Embedded { focus: Pane::Right } = &app.screen {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            if let Some(idx) = app.active_agent_idx {
                                if let Some(agent) = app.agents.get_mut(idx) {
                                    agent.term.scroll_up(3);
                                }
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if let Some(idx) = app.active_agent_idx {
                                if let Some(agent) = app.agents.get_mut(idx) {
                                    agent.term.scroll_down(3);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            if let Event::Key(key) = ev {
                // ── Global: Ctrl+Space toggles agent command palette ──
                if key.code == KeyCode::Char(' ') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    if !app.agents.is_empty() {
                        app.agent_palette_open = !app.agent_palette_open;
                        if app.agent_palette_open {
                            app.agent_palette_state.select(app.active_agent_idx.or(Some(0)));
                        }
                    }
                }
                // ── Agent palette key handling (consumes all keys when open) ──
                else if app.agent_palette_open {
                    match key.code {
                        KeyCode::Esc => {
                            app.agent_palette_open = false;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let i = app.agent_palette_state.selected().unwrap_or(0);
                            app.agent_palette_state.select(Some(i.saturating_sub(1)));
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let len = app.agents.len();
                            if len > 0 {
                                let i = app.agent_palette_state.selected().unwrap_or(0);
                                app.agent_palette_state.select(Some((i + 1).min(len - 1)));
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(i) = app.agent_palette_state.selected() {
                                if i < app.agents.len() {
                                    app.active_agent_idx = Some(i);
                                    app.agents_state.select(Some(i));
                                    app.screen = Screen::Embedded { focus: Pane::Right };
                                    app.agent_palette_open = false;
                                }
                            }
                        }
                        KeyCode::Char(c @ '1'..='9') => {
                            let idx = (c as usize) - ('1' as usize);
                            if idx < app.agents.len() {
                                app.active_agent_idx = Some(idx);
                                app.agents_state.select(Some(idx));
                                app.screen = Screen::Embedded { focus: Pane::Right };
                                app.agent_palette_open = false;
                            }
                        }
                        KeyCode::Char('x') => {
                            if let Some(i) = app.agent_palette_state.selected() {
                                if i < app.agents.len() {
                                    app.agents.remove(i);
                                    if app.agents.is_empty() {
                                        app.active_agent_idx = None;
                                        app.agent_palette_open = false;
                                    } else {
                                        let new_sel = i.min(app.agents.len() - 1);
                                        app.agent_palette_state.select(Some(new_sel));
                                        if app.active_agent_idx == Some(i) {
                                            app.active_agent_idx = Some(new_sel);
                                        } else if let Some(ref mut ai) = app.active_agent_idx {
                                            if *ai > i { *ai -= 1; }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {} // palette consumes all other keys
                    }
                }
                else { match app.screen.clone() {
                    // ── Projects screen ──────────────────────
                    Screen::Projects => {
                        if app.searching {
                            // Search mode key handling
                            match key.code {
                                KeyCode::Esc => {
                                    // Cancel search, restore full list
                                    app.searching = false;
                                    app.search_query.clear();
                                    app.filtered_indices = (0..app.projects.len()).collect();
                                    if !app.projects.is_empty() {
                                        app.projects_state.select(Some(0));
                                    }
                                }
                                KeyCode::Enter => {
                                    // Confirm search, open selected project
                                    app.searching = false;
                                    if let Some(proj_i) = app.selected_project_idx() {
                                        app.load_sessions(proj_i);
                                    }
                                }
                                KeyCode::Backspace => {
                                    app.search_query.pop();
                                    app.update_search_filter();
                                }
                                KeyCode::Char(c) => {
                                    app.search_query.push(c);
                                    app.update_search_filter();
                                }
                                KeyCode::Down => {
                                    let len = app.filtered_indices.len();
                                    if len > 0 {
                                        let i = app.projects_state.selected().unwrap_or(0);
                                        app.projects_state.select(Some((i + 1).min(len - 1)));
                                    }
                                }
                                KeyCode::Up => {
                                    let i = app.projects_state.selected().unwrap_or(0);
                                    app.projects_state.select(Some(i.saturating_sub(1)));
                                }
                                _ => {}
                            }
                        } else {
                            // Normal mode
                            match (key.code, key.modifiers) {
                                (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                    app.should_quit = true;
                                }
                                (KeyCode::Esc, _) => app.should_quit = true,
                                (KeyCode::Char('/'), _) => {
                                    // Enter search mode
                                    app.searching = true;
                                    app.search_query.clear();
                                    app.filtered_indices = (0..app.projects.len()).collect();
                                }
                                (KeyCode::Down | KeyCode::Char('j'), _) => {
                                    let len = if app.search_query.is_empty() { app.projects.len() } else { app.filtered_indices.len() };
                                    if len > 0 {
                                        let i = app.projects_state.selected().unwrap_or(0);
                                        app.projects_state.select(Some((i + 1).min(len - 1)));
                                    }
                                }
                                (KeyCode::Up | KeyCode::Char('k'), _) => {
                                    let i = app.projects_state.selected().unwrap_or(0);
                                    app.projects_state.select(Some(i.saturating_sub(1)));
                                }
                                (KeyCode::Enter | KeyCode::Right, _) => {
                                    if let Some(proj_i) = app.selected_project_idx() {
                                        app.load_sessions(proj_i);
                                    }
                                }
                                (KeyCode::Char('g'), _) => {
                                    let len = if app.search_query.is_empty() { app.projects.len() } else { app.filtered_indices.len() };
                                    if len > 0 { app.projects_state.select(Some(0)); }
                                }
                                (KeyCode::Char('G'), _) => {
                                    let len = if app.search_query.is_empty() { app.projects.len() } else { app.filtered_indices.len() };
                                    if len > 0 { app.projects_state.select(Some(len - 1)); }
                                }
                                (KeyCode::Tab, _) => {
                                    app.project_expanded = !app.project_expanded;
                                }
                                (KeyCode::Char('D'), _) => {
                                    app.danger_mode = !app.danger_mode;
                                }
                                (KeyCode::Char(c @ '1'..='9'), _) => {
                                    let idx = (c as usize) - ('1' as usize);
                                    app.jump_to_pin(idx);
                                }
                                _ => {}
                            }
                        }
                    },

                    // ── Session split screen ────────────────
                    Screen::SessionSplit { project_idx, focus } => {
                        if app.timeline_searching && focus == Pane::Right {
                            // Timeline search mode
                            match key.code {
                                KeyCode::Esc => {
                                    app.timeline_searching = false;
                                    app.timeline_search_query.clear();
                                    app.timeline_filtered_indices.clear();
                                    // Restore selection
                                    if !app.turns.is_empty() {
                                        app.timeline_state.select(Some(0));
                                    }
                                }
                                KeyCode::Enter => {
                                    app.timeline_searching = false;
                                    // Keep filter active, jump to selected match
                                }
                                KeyCode::Backspace => {
                                    app.timeline_search_query.pop();
                                    app.update_timeline_search_filter();
                                }
                                KeyCode::Char(c) => {
                                    app.timeline_search_query.push(c);
                                    app.update_timeline_search_filter();
                                }
                                KeyCode::Down => {
                                    let len = app.timeline_filtered_indices.len();
                                    if len > 0 {
                                        let i = app.timeline_state.selected().unwrap_or(0);
                                        app.timeline_state.select(Some((i + 1).min(len - 1)));
                                    }
                                }
                                KeyCode::Up => {
                                    let i = app.timeline_state.selected().unwrap_or(0);
                                    app.timeline_state.select(Some(i.saturating_sub(1)));
                                }
                                _ => {}
                            }
                        } else {
                            match (key.code, key.modifiers) {
                                (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                    app.should_quit = true;
                                }
                                (KeyCode::Esc | KeyCode::Backspace, _) => {
                                    if !app.timeline_search_query.is_empty() && focus == Pane::Right {
                                        // Clear search filter first
                                        app.timeline_search_query.clear();
                                        app.timeline_filtered_indices.clear();
                                        if !app.turns.is_empty() {
                                            app.timeline_state.select(Some(0));
                                        }
                                    } else if focus == Pane::Right {
                                        app.screen = Screen::SessionSplit { project_idx, focus: Pane::Left };
                                    } else {
                                        app.screen = Screen::Projects;
                                    }
                                }
                                (KeyCode::Char('/'), _) if focus == Pane::Right => {
                                    app.timeline_searching = true;
                                    app.timeline_search_query.clear();
                                    app.timeline_filtered_indices = (0..app.turns.len()).collect();
                                }
                                (KeyCode::Tab, _) => {
                                    let new_focus = if focus == Pane::Left { Pane::Right } else { Pane::Left };
                                    app.screen = Screen::SessionSplit { project_idx, focus: new_focus };
                                }
                                (KeyCode::Left, _) => {
                                    if focus == Pane::Right {
                                        app.screen = Screen::SessionSplit { project_idx, focus: Pane::Left };
                                    } else {
                                        app.screen = Screen::Projects;
                                    }
                                }
                                (KeyCode::Right, _) => {
                                    if focus == Pane::Left {
                                        app.screen = Screen::SessionSplit { project_idx, focus: Pane::Right };
                                    }
                                }
                                (KeyCode::Down | KeyCode::Char('j'), _) => {
                                    if focus == Pane::Left {
                                        let len = app.sessions.len();
                                        if len > 0 {
                                            let i = app.sessions_state.selected().unwrap_or(0);
                                            app.sessions_state.select(Some((i + 1).min(len - 1)));
                                            app.update_preview();
                                        }
                                    } else {
                                        let len = if !app.timeline_search_query.is_empty() { app.timeline_filtered_indices.len() } else { app.turns.len() };
                                        if len > 0 {
                                            let i = app.timeline_state.selected().unwrap_or(0);
                                            app.timeline_state.select(Some((i + 1).min(len - 1)));
                                        }
                                    }
                                }
                                (KeyCode::Up | KeyCode::Char('k'), _) => {
                                    if focus == Pane::Left {
                                        let i = app.sessions_state.selected().unwrap_or(0);
                                        app.sessions_state.select(Some(i.saturating_sub(1)));
                                        app.update_preview();
                                    } else {
                                        let i = app.timeline_state.selected().unwrap_or(0);
                                        app.timeline_state.select(Some(i.saturating_sub(1)));
                                    }
                                }
                                (KeyCode::Enter, _) => {
                                    if focus == Pane::Left {
                                        app.screen = Screen::SessionSplit { project_idx, focus: Pane::Right };
                                    } else {
                                        // Toggle expand — map to real turn index if filtered
                                        let real_i = if !app.timeline_search_query.is_empty() {
                                            app.selected_turn_real_idx()
                                        } else {
                                            app.timeline_state.selected()
                                        };
                                        if let Some(i) = real_i {
                                            if i < app.expanded.len() {
                                                app.expanded[i] = !app.expanded[i];
                                            }
                                        }
                                    }
                                }
                                (KeyCode::Char('e'), _) if focus == Pane::Right => {
                                    for e in &mut app.expanded { *e = true; }
                                }
                                (KeyCode::Char('c'), _) if focus == Pane::Right => {
                                    for e in &mut app.expanded { *e = false; }
                                }
                                (KeyCode::Char('f'), _) if focus == Pane::Right => {
                                    app.screen = Screen::Timeline;
                                }
                                (KeyCode::Char('g'), _) => {
                                    if focus == Pane::Left {
                                        if !app.sessions.is_empty() {
                                            app.sessions_state.select(Some(0));
                                            app.update_preview();
                                        }
                                    } else {
                                        let len = if !app.timeline_search_query.is_empty() { app.timeline_filtered_indices.len() } else { app.turns.len() };
                                        if len > 0 { app.timeline_state.select(Some(0)); }
                                    }
                                }
                                (KeyCode::Char('G'), _) => {
                                    if focus == Pane::Left {
                                        if !app.sessions.is_empty() {
                                            app.sessions_state.select(Some(app.sessions.len() - 1));
                                            app.update_preview();
                                        }
                                    } else {
                                        let len = if !app.timeline_search_query.is_empty() { app.timeline_filtered_indices.len() } else { app.turns.len() };
                                        if len > 0 { app.timeline_state.select(Some(len - 1)); }
                                    }
                                }
                                (KeyCode::Char('s'), _) => {
                                    app.toggle_pin();
                                }
                                (KeyCode::Char('D'), _) => {
                                    app.danger_mode = !app.danger_mode;
                                }
                                (KeyCode::Char('r'), _) => {
                                    if let Some(_sid) = app.current_session_id() {
                                        let a = terminal.size().unwrap_or_default();
                                        let danger = app.danger_mode;
                                        open_embedded(&mut app, false, danger, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                                    }
                                }
                                (KeyCode::Char('F'), _) => {
                                    if let Some(_sid) = app.current_session_id() {
                                        let a = terminal.size().unwrap_or_default();
                                        let danger = app.danger_mode;
                                        open_embedded(&mut app, true, danger, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                                    }
                                }
                                (KeyCode::Char(c @ '1'..='9'), _) => {
                                    let idx = (c as usize) - ('1' as usize);
                                    app.jump_to_pin(idx);
                                }
                                (KeyCode::Char('a'), _) => {
                                    if !app.agents.is_empty() {
                                        app.left_pane_mode = LeftPaneMode::Agents;
                                        if app.agents_state.selected().is_none() {
                                            app.agents_state.select(app.active_agent_idx.or(Some(0)));
                                        }
                                        app.screen = Screen::Embedded { focus: Pane::Left };
                                    }
                                }
                                (KeyCode::Char('o'), _) => {
                                    // Open repo URL in browser
                                    if let Some(ref info) = app.info {
                                        if let Some(url) = get_git_remote_url(&info.project_dir) {
                                            let _ = std::process::Command::new("open").arg(&url).spawn();
                                        }
                                    }
                                }
                                (KeyCode::Char('y'), _) => {
                                    // Copy repo URL to clipboard
                                    if let Some(ref info) = app.info {
                                        if let Some(url) = get_git_remote_url(&info.project_dir) {
                                            if let Ok(mut child) = std::process::Command::new("pbcopy")
                                                .stdin(std::process::Stdio::piped())
                                                .spawn()
                                            {
                                                if let Some(ref mut stdin) = child.stdin {
                                                    use std::io::Write;
                                                    let _ = stdin.write_all(url.as_bytes());
                                                }
                                                let _ = child.wait();
                                                app.last_error = None;
                                            }
                                        }
                                    }
                                }
                                (KeyCode::Char('p'), _) => {
                                    app.screen = Screen::Projects;
                                }
                                _ => {}
                            }
                        }
                    },

                    // ── Full timeline screen ────────────────
                    Screen::Timeline => match (key.code, key.modifiers) {
                        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            app.should_quit = true;
                        }
                        (KeyCode::Esc | KeyCode::Backspace, _) => {
                            // Go back to projects (or split if we came from there)
                            app.screen = Screen::Projects;
                        }
                        (KeyCode::Down | KeyCode::Char('j'), _) => {
                            let len = app.turns.len();
                            if len > 0 {
                                let i = app.timeline_state.selected().unwrap_or(0);
                                app.timeline_state.select(Some((i + 1).min(len - 1)));
                            }
                        }
                        (KeyCode::Up | KeyCode::Char('k'), _) => {
                            let i = app.timeline_state.selected().unwrap_or(0);
                            app.timeline_state.select(Some(i.saturating_sub(1)));
                        }
                        (KeyCode::Enter | KeyCode::Char(' '), _) => {
                            if let Some(i) = app.timeline_state.selected() {
                                if i < app.expanded.len() {
                                    app.expanded[i] = !app.expanded[i];
                                }
                            }
                        }
                        (KeyCode::Char('e'), _) => { for e in &mut app.expanded { *e = true; } }
                        (KeyCode::Char('c'), _) => { for e in &mut app.expanded { *e = false; } }
                        (KeyCode::Char('g'), _) => {
                            if !app.turns.is_empty() { app.timeline_state.select(Some(0)); }
                        }
                        (KeyCode::Char('G'), _) => {
                            if !app.turns.is_empty() { app.timeline_state.select(Some(app.turns.len() - 1)); }
                        }
                        (KeyCode::Char('s'), _) => {
                            app.toggle_pin();
                        }
                        (KeyCode::Char('D'), _) => {
                            app.danger_mode = !app.danger_mode;
                        }
                        (KeyCode::Char('r'), _) => {
                            // Resume this session in Claude Code
                            if let Some(_sid) = app.current_session_id() {
                                let a = terminal.size().unwrap_or_default();
                                let danger = app.danger_mode;
                                open_embedded(&mut app, false, danger, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                            }
                        }
                        (KeyCode::Char('F'), _) => {
                            // Fork this session in Claude Code
                            if let Some(_sid) = app.current_session_id() {
                                let a = terminal.size().unwrap_or_default();
                                let danger = app.danger_mode;
                                open_embedded(&mut app, true, danger, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                            }
                        }
                        (KeyCode::Char(c @ '1'..='9'), _) => {
                            let idx = (c as usize) - ('1' as usize);
                            app.jump_to_pin(idx);
                        }
                        (KeyCode::Char('R'), _) => app.reload_timeline(),
                        (KeyCode::Char('p'), _) => { app.screen = Screen::Projects; }
                        _ => {}
                    },

                    // ── Embedded Claude Code ─────────────────
                    Screen::Embedded { focus } => {
                        if focus == Pane::Right {
                            // Escape hatch: double-Esc (two Esc within 400ms) or Ctrl+] to exit
                            let should_escape = if key.code == KeyCode::Esc {
                                if let Some(last) = app.last_esc_time {
                                    if last.elapsed() < Duration::from_millis(400) {
                                        app.last_esc_time = None;
                                        true // double-Esc detected!
                                    } else {
                                        app.last_esc_time = Some(std::time::Instant::now());
                                        // Forward single Esc to Claude Code
                                        if let Some(idx) = app.active_agent_idx {
                                            if let Some(agent) = app.agents.get_mut(idx) {
                                                agent.term.send_key(key);
                                            }
                                        }
                                        false
                                    }
                                } else {
                                    app.last_esc_time = Some(std::time::Instant::now());
                                    // Forward single Esc to Claude Code
                                    if let Some(idx) = app.active_agent_idx {
                                        if let Some(agent) = app.agents.get_mut(idx) {
                                            agent.term.send_key(key);
                                        }
                                    }
                                    false
                                }
                            } else if (key.code == KeyCode::Char(']') || key.code == KeyCode::Char('\\'))
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                true
                            } else {
                                app.last_esc_time = None;
                                false
                            };

                            if should_escape {
                                app.screen = Screen::Embedded { focus: Pane::Left };
                            } else if key.code != KeyCode::Esc {
                                // Snap viewport to bottom and forward key to Claude Code
                                if let Some(idx) = app.active_agent_idx {
                                    if let Some(agent) = app.agents.get_mut(idx) {
                                        agent.term.snap_to_bottom();
                                        agent.term.send_key(key);
                                    }
                                }
                            }
                        } else {
                            // Left pane: carousel (timeline or agents)
                            match (key.code, key.modifiers) {
                                (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                    app.should_quit = true;
                                }
                                (KeyCode::Esc, _) => {
                                    // Don't kill agents, just go back
                                    app.left_pane_mode = LeftPaneMode::Sessions;
                                    app.screen = Screen::Projects;
                                }
                                (KeyCode::Char('a'), _) => {
                                    // Toggle carousel mode
                                    app.left_pane_mode = if app.left_pane_mode == LeftPaneMode::Sessions {
                                        // Initialize agents selection if needed
                                        if !app.agents.is_empty() && app.agents_state.selected().is_none() {
                                            app.agents_state.select(app.active_agent_idx.or(Some(0)));
                                        }
                                        LeftPaneMode::Agents
                                    } else {
                                        LeftPaneMode::Sessions
                                    };
                                }
                                (KeyCode::Tab, _) | (KeyCode::Right, _) => {
                                    app.screen = Screen::Embedded { focus: Pane::Right };
                                }
                                (KeyCode::Down | KeyCode::Char('j'), _) => {
                                    if app.left_pane_mode == LeftPaneMode::Agents {
                                        let len = app.agents.len();
                                        if len > 0 {
                                            let i = app.agents_state.selected().unwrap_or(0);
                                            app.agents_state.select(Some((i + 1).min(len - 1)));
                                        }
                                    } else {
                                        let len = app.turns.len();
                                        if len > 0 {
                                            let i = app.timeline_state.selected().unwrap_or(0);
                                            app.timeline_state.select(Some((i + 1).min(len - 1)));
                                        }
                                    }
                                }
                                (KeyCode::Up | KeyCode::Char('k'), _) => {
                                    if app.left_pane_mode == LeftPaneMode::Agents {
                                        let i = app.agents_state.selected().unwrap_or(0);
                                        app.agents_state.select(Some(i.saturating_sub(1)));
                                    } else {
                                        let i = app.timeline_state.selected().unwrap_or(0);
                                        app.timeline_state.select(Some(i.saturating_sub(1)));
                                    }
                                }
                                (KeyCode::Enter | KeyCode::Char(' '), _) => {
                                    if app.left_pane_mode == LeftPaneMode::Agents {
                                        // Switch active agent
                                        if let Some(i) = app.agents_state.selected() {
                                            if i < app.agents.len() {
                                                app.active_agent_idx = Some(i);
                                                app.screen = Screen::Embedded { focus: Pane::Right };
                                            }
                                        }
                                    } else {
                                        // Toggle expand in timeline
                                        if let Some(i) = app.timeline_state.selected() {
                                            if i < app.expanded.len() {
                                                app.expanded[i] = !app.expanded[i];
                                            }
                                        }
                                    }
                                }
                                (KeyCode::Char('x'), _) if app.left_pane_mode == LeftPaneMode::Agents => {
                                    // Kill selected agent
                                    if let Some(i) = app.agents_state.selected() {
                                        if i < app.agents.len() {
                                            app.agents.remove(i);
                                            // Fix active_agent_idx
                                            if app.agents.is_empty() {
                                                app.active_agent_idx = None;
                                                app.left_pane_mode = LeftPaneMode::Sessions;
                                                app.screen = Screen::Projects;
                                            } else {
                                                let new_sel = i.min(app.agents.len() - 1);
                                                app.agents_state.select(Some(new_sel));
                                                if app.active_agent_idx == Some(i) {
                                                    app.active_agent_idx = Some(new_sel);
                                                } else if let Some(ref mut ai) = app.active_agent_idx {
                                                    if *ai > i { *ai -= 1; }
                                                }
                                            }
                                        }
                                    }
                                }
                                (KeyCode::Char('e'), _) if app.left_pane_mode == LeftPaneMode::Sessions => {
                                    for e in &mut app.expanded { *e = true; }
                                }
                                (KeyCode::Char('c'), _) if app.left_pane_mode == LeftPaneMode::Sessions => {
                                    for e in &mut app.expanded { *e = false; }
                                }
                                _ => {}
                            }
                        }
                    },
                }
            } // else (palette not open)
            }
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    if kb_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}
