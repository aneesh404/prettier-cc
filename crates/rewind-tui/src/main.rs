mod chat;
mod embedded;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
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
    path: PathBuf,
    session_count: usize,
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

    // Double-Esc detection: timestamp of last Esc press
    last_esc_time: Option<std::time::Instant>,
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
            last_esc_time: None,
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
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let mut session_count = 0;
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
                    projects.push(ProjectEntry {
                        display_name,
                        path,
                        session_count,
                        latest_modified: latest,
                    });
                }
            }
        }
        projects.sort_by(|a, b| b.latest_modified.cmp(&a.latest_modified));
        self.projects = projects;
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
            }
            Err(e) => {
                self.info = None;
                self.turns.clear();
                self.expanded.clear();
                self.last_error = Some(e);
            }
        }
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
fn open_embedded(app: &mut App, fork: bool, rows: u16, cols: u16) {
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

    let sid_short = if info.session_id.len() > 8 {
        info.session_id[..8].to_string()
    } else {
        info.session_id.clone()
    };
    let label = if fork {
        format!("Fork {sid_short}")
    } else {
        format!("Resume {sid_short}")
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

fn build_timeline_items(turns: &[Turn], expanded: &[bool], selected: Option<usize>) -> Vec<ListItem<'static>> {
    let mut items = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        let is_expanded = expanded.get(i).copied().unwrap_or(false);
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

// ── Render: Projects ────────────────────────────────────────────────────────

fn render_projects(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" ⏪ Rewind Rail ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{} projects", app.projects.len()), Style::default().fg(Color::Cyan)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = app.projects.iter().map(|p| {
        let age = relative_time(p.latest_modified);
        let sl = if p.session_count == 1 { "1 session" } else { &format!("{} sessions", p.session_count) };
        ListItem::new(vec![
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(&p.display_name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![Span::styled(format!("     {sl} · {age}"), Style::default().fg(Color::DarkGray))]),
            Line::from(""),
        ])
    }).collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT).border_style(Style::default().fg(Color::DarkGray)).padding(Padding::new(0, 0, 1, 0)))
        .highlight_style(Style::default().bg(Color::Rgb(20, 50, 60)))
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, chunks[1], &mut app.projects_state);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, chunks[2]);
}

// ── Render: Session Split ───────────────────────────────────────────────────

fn render_session_split(frame: &mut ratatui::Frame, app: &mut App, project_idx: usize, focus: Pane) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let project_path = if project_idx < app.projects.len() {
        app.projects[project_idx].display_name.clone()
    } else {
        "?".to_string()
    };

    // Header — project path + session count
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Rewind Rail ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&project_path, Style::default().fg(Color::Rgb(100, 150, 180))),
        Span::styled(format!(" │ {} sessions", app.sessions.len()), Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, outer[0]);

    // 3-pane layout: sessions (30%) | timeline (45%) | details (25%)
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(45), Constraint::Percentage(25)])
        .split(outer[1]);

    // ── Left pane: sessions list ──
    let left_border_color = if focus == Pane::Left { Color::Cyan } else { Color::DarkGray };

    let session_items: Vec<ListItem> = app.sessions.iter().enumerate().map(|(i, s)| {
        let head_label = if i == 0 { "  HEAD" } else { "" };
        let marker = if i == 0 { "●" } else { "○" };
        let mc = if i == 0 { Color::Green } else { Color::DarkGray };
        let date = date_from_iso(&s.info.started_at);
        let time = s.info.started_at.as_ref().and_then(|t| t.get(11..16)).unwrap_or("?");

        ListItem::new(vec![
            Line::from(vec![
                Span::styled(format!("  {marker} "), Style::default().fg(mc)),
                Span::styled(&s.session_id, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(head_label, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
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
        frame.render_widget(empty, panes[1]);
    } else {
        let sel = if focus == Pane::Right { app.timeline_state.selected() } else { None };
        let items = build_timeline_items(&app.turns, &app.expanded, sel);
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
        frame.render_stateful_widget(timeline_list, panes[1], &mut app.timeline_state);
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
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
            Span::styled("F", Style::default().fg(Color::Yellow)),
            Span::styled(" fork  ", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan)),
            Span::styled("/", Style::default().fg(Color::DarkGray)),
            Span::styled("←", Style::default().fg(Color::Cyan)),
            Span::styled(" sessions  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::styled(" expand  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
            Span::styled("F", Style::default().fg(Color::Yellow)),
            Span::styled(" fork  ", Style::default().fg(Color::DarkGray)),
        ]
    };

    let mut footer_spans = vec![
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
    ];
    footer_spans.extend(focus_hint);
    if !app.agents.is_empty() {
        footer_spans.extend(vec![
            Span::styled("a", Style::default().fg(Color::Yellow)),
            Span::styled(format!(" agents({})  ", app.agents.len()), Style::default().fg(Color::DarkGray)),
        ]);
    }
    footer_spans.extend(vec![
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]);

    let footer = Paragraph::new(Line::from(footer_spans))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, outer[2]);
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

    let primary_tools = top_tools(&app.turns, 4);
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
        Line::from(vec![
            Span::styled("  Primary Tools:  ", label_style),
        ]),
    ];

    // Wrap primary tools across lines if needed
    for tool_name in &primary_tools {
        lines.push(Line::from(vec![
            Span::styled("    ", label_style),
            Span::styled(tool_name, Style::default().fg(tool_color(tool_name))),
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let info = app.info.as_ref();
    let pn = info.map(|i| i.project_dir.split('/').last().unwrap_or(&i.project_dir)).unwrap_or("?");
    let br = info.and_then(|i| i.git_branch.as_deref()).unwrap_or("—");
    let tk = info.map(|i| format_tokens(i.total_input_tokens, i.total_output_tokens)).unwrap_or_default();

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" ⏪ Rewind Rail ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(pn, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" ({br}) "), Style::default().fg(Color::DarkGray)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&tk, Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(header, chunks[0]);

    let items = build_timeline_items(&app.turns, &app.expanded, app.timeline_state.selected());
    let list = List::new(items)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT).border_style(Style::default().fg(Color::DarkGray)).padding(Padding::new(0, 0, 1, 0)))
        .highlight_style(Style::default().bg(Color::Rgb(20, 50, 60)));
    frame.render_stateful_widget(list, chunks[1], &mut app.timeline_state);

    let tc = app.turns.len();
    let sel = app.timeline_state.selected().map(|s| s + 1).unwrap_or(0);
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(format!(" {sel}/{tc} "), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::styled(" expand  ", Style::default().fg(Color::DarkGray)),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
        Span::styled("F", Style::default().fg(Color::Yellow)),
        Span::styled(" fork  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, chunks[2]);
}

// ── Render: Embedded Claude Code ─────────────────────────────────────────────

fn render_embedded(frame: &mut ratatui::Frame, app: &mut App, focus: Pane) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(1)])
        .split(area);

    // Determine active agent status
    let active_agent = app.active_agent_idx.and_then(|i| app.agents.get_mut(i));
    let (agent_label, running) = match active_agent {
        Some(a) => (a.label.clone(), a.term.is_running()),
        None => ("—".to_string(), false),
    };
    let (status, status_color) = if running { ("RUNNING", Color::Green) } else { ("EXITED", Color::Red) };
    let agent_count = app.agents.len();

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Rewind Rail ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
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
        .split(outer[1]);

    if app.left_pane_mode == LeftPaneMode::Agents {
        // ── Left pane: agents list ──
        render_agents_list(frame, app, panes[0], focus == Pane::Left);
    } else {
        // ── Left pane: timeline ──
        let left_border_color = if focus == Pane::Left { Color::Cyan } else { Color::DarkGray };
        let sel = if focus == Pane::Left { app.timeline_state.selected() } else { None };
        let items = build_timeline_items(&app.turns, &app.expanded, sel);
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

    if let Some(idx) = app.active_agent_idx {
        if let Some(agent) = app.agents.get(idx) {
            let lines = agent.term.render_lines(inner.height, inner.width);
            let para = Paragraph::new(lines);
            frame.render_widget(para, inner);
        }
    }

    // Footer
    let mode_label = if app.left_pane_mode == LeftPaneMode::Agents { "Timeline" } else { "Agents" };
    let hint = if focus == Pane::Right {
        format!(" Esc Esc → left pane │ Ctrl+] also works │ a → {mode_label} │ All keys → Claude ")
    } else if app.left_pane_mode == LeftPaneMode::Agents {
        format!(" ↑↓ nav │ Enter switch │ a → {mode_label} │ Tab/→ terminal │ x kill │ Esc close ")
    } else {
        format!(" ↑↓ nav │ Enter expand │ a → {mode_label} │ Tab/→ terminal │ Esc close │ q quit ")
    };
    let footer = Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, outer[2]);
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

// ── Main render dispatch ────────────────────────────────────────────────────

fn ui(frame: &mut ratatui::Frame, app: &mut App) {
    match app.screen.clone() {
        Screen::Projects => render_projects(frame, app),
        Screen::SessionSplit { project_idx, focus } => render_session_split(frame, app, project_idx, focus),
        Screen::Timeline => render_timeline(frame, app),
        Screen::Embedded { focus } => render_embedded(frame, app, focus),
    }
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
            if let Event::Key(key) = event::read()? {
                match app.screen.clone() {
                    // ── Projects screen ──────────────────────
                    Screen::Projects => match (key.code, key.modifiers) {
                        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            app.should_quit = true;
                        }
                        (KeyCode::Esc, _) => app.should_quit = true,
                        (KeyCode::Down | KeyCode::Char('j'), _) => {
                            let len = app.projects.len();
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
                            if let Some(i) = app.projects_state.selected() {
                                app.load_sessions(i);
                            }
                        }
                        (KeyCode::Char('g'), _) => {
                            if !app.projects.is_empty() { app.projects_state.select(Some(0)); }
                        }
                        (KeyCode::Char('G'), _) => {
                            if !app.projects.is_empty() { app.projects_state.select(Some(app.projects.len() - 1)); }
                        }
                        _ => {}
                    },

                    // ── Session split screen ────────────────
                    Screen::SessionSplit { project_idx, focus } => match (key.code, key.modifiers) {
                        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            app.should_quit = true;
                        }
                        (KeyCode::Esc | KeyCode::Backspace, _) => {
                            if focus == Pane::Right {
                                app.screen = Screen::SessionSplit { project_idx, focus: Pane::Left };
                            } else {
                                app.screen = Screen::Projects;
                            }
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
                                let len = app.turns.len();
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
                                // Switch focus to right pane
                                app.screen = Screen::SessionSplit { project_idx, focus: Pane::Right };
                            } else {
                                // Toggle expand in timeline
                                if let Some(i) = app.timeline_state.selected() {
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
                            // Full screen timeline
                            app.screen = Screen::Timeline;
                        }
                        (KeyCode::Char('g'), _) => {
                            if focus == Pane::Left {
                                if !app.sessions.is_empty() {
                                    app.sessions_state.select(Some(0));
                                    app.update_preview();
                                }
                            } else if !app.turns.is_empty() {
                                app.timeline_state.select(Some(0));
                            }
                        }
                        (KeyCode::Char('G'), _) => {
                            if focus == Pane::Left {
                                if !app.sessions.is_empty() {
                                    app.sessions_state.select(Some(app.sessions.len() - 1));
                                    app.update_preview();
                                }
                            } else if !app.turns.is_empty() {
                                app.timeline_state.select(Some(app.turns.len() - 1));
                            }
                        }
                        (KeyCode::Char('r'), _) => {
                            // Resume this session in Claude Code (user can Esc+Esc to rewind)
                            if let Some(sid) = app.current_session_id() {
                                let a = terminal.size().unwrap_or_default();
                                open_embedded(&mut app, false, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                            }
                        }
                        (KeyCode::Char('F'), _) => {
                            // Fork this session in Claude Code
                            if let Some(sid) = app.current_session_id() {
                                let a = terminal.size().unwrap_or_default();
                                open_embedded(&mut app, true, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                            }
                        }
                        (KeyCode::Char('a'), _) => {
                            // Switch to agents view (if agents exist, jump to embedded screen with agents carousel)
                            if !app.agents.is_empty() {
                                app.left_pane_mode = LeftPaneMode::Agents;
                                if app.agents_state.selected().is_none() {
                                    app.agents_state.select(app.active_agent_idx.or(Some(0)));
                                }
                                app.screen = Screen::Embedded { focus: Pane::Left };
                            }
                        }
                        (KeyCode::Char('p'), _) => {
                            app.screen = Screen::Projects;
                        }
                        _ => {}
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
                        (KeyCode::Char('r'), _) => {
                            // Resume this session in Claude Code
                            if let Some(sid) = app.current_session_id() {
                                let a = terminal.size().unwrap_or_default();
                                open_embedded(&mut app, false, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                            }
                        }
                        (KeyCode::Char('F'), _) => {
                            // Fork this session in Claude Code
                            if let Some(sid) = app.current_session_id() {
                                let a = terminal.size().unwrap_or_default();
                                open_embedded(&mut app, true, a.height.saturating_sub(6), (a.width * 70 / 100).saturating_sub(2));
                            }
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
                                // Forward non-Esc keys to Claude Code
                                if let Some(idx) = app.active_agent_idx {
                                    if let Some(agent) = app.agents.get_mut(idx) {
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
            }
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}
