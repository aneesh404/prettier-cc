use continuum_core::transcript::parse_transcript;
use std::path::{Path, PathBuf};

// ── ANSI helpers ────────────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const WHITE: &str = "\x1b[37m";
const BG_BLUE: &str = "\x1b[48;5;24m";
const BG_DIM: &str = "\x1b[48;5;236m";

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

fn tool_color(name: &str) -> &str {
    match name {
        "Write" | "Edit" => GREEN,
        "Read" | "Glob" | "Grep" => CYAN,
        "Bash" => YELLOW,
        "Agent" => MAGENTA,
        _ => DIM,
    }
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

// ── Find transcripts ────────────────────────────────────────────────────────

fn claude_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".claude/projects")
}

/// Find transcript files for the current project or all projects.
fn find_transcripts(project: Option<&str>) -> Vec<PathBuf> {
    let base = claude_projects_dir();
    let mut results = Vec::new();

    let target_dir = if let Some(proj) = project {
        // Direct project path
        base.join(proj)
    } else {
        // Try to detect from CWD
        let cwd = std::env::current_dir().unwrap_or_default();
        let cwd_str = cwd.to_string_lossy().replace('/', "-");
        base.join(&cwd_str)
    };

    if target_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&target_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && !path.to_string_lossy().contains("subagents")
                {
                    results.push(path);
                }
            }
        }
    }

    // Sort by modification time (newest first)
    results.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });

    results
}

/// List all project directories that have transcripts.
fn find_all_projects() -> Vec<(String, PathBuf, usize)> {
    let base = claude_projects_dir();
    let mut projects = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let count = std::fs::read_dir(&path)
                    .map(|e| {
                        e.filter(|f| {
                            f.as_ref()
                                .ok()
                                .and_then(|f| {
                                    f.path()
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .map(|e| e == "jsonl")
                                })
                                .unwrap_or(false)
                        })
                        .count()
                    })
                    .unwrap_or(0);

                if count > 0 {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    projects.push((name, path, count));
                }
            }
        }
    }

    projects.sort_by(|a, b| b.2.cmp(&a.2));
    projects
}

// ── Commands ────────────────────────────────────────────────────────────────

fn cmd_sessions(project: Option<&str>) {
    let transcripts = find_transcripts(project);

    if transcripts.is_empty() {
        // Fall back to listing all projects
        let projects = find_all_projects();
        if projects.is_empty() {
            println!("{DIM}No Claude Code sessions found.{RESET}");
            return;
        }

        println!("{BOLD}All Projects{RESET}");
        println!();
        for (name, _path, count) in &projects {
            let display_name = name.replace('-', "/");
            println!("  {GREEN}●{RESET} {BOLD}{display_name}{RESET}  {DIM}({count} sessions){RESET}");
        }
        println!();
        println!("{DIM}Use: continuum timeline --project <name>{RESET}");
        return;
    }

    println!("{BOLD}Sessions{RESET} {DIM}(newest first){RESET}");
    println!();

    for (i, path) in transcripts.iter().enumerate().take(10) {
        match parse_transcript(path) {
            Ok((info, _turns)) => {
                let branch = info.git_branch.as_deref().unwrap_or("—");
                let started = info.started_at.as_ref().and_then(|s| s.get(..16)).unwrap_or("?");
                let tokens = format_tokens(info.total_input_tokens, info.total_output_tokens);

                let session_short = if info.session_id.len() > 8 {
                    &info.session_id[..8]
                } else {
                    &info.session_id
                };

                let marker = if i == 0 {
                    format!("{GREEN}●{RESET}")
                } else {
                    format!("{DIM}○{RESET}")
                };

                println!(
                    "  {marker} {BOLD}{session_short}{RESET}  {DIM}branch:{RESET}{CYAN}{branch}{RESET}  {DIM}{started}{RESET}"
                );
                println!(
                    "    {info_prompts} prompts · {info_tools} tool calls · {tokens}",
                    info_prompts = info.prompt_count,
                    info_tools = info.tool_call_count,
                );
                println!();
            }
            Err(e) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                println!("  {RED}✗{RESET} {name}: {DIM}{e}{RESET}");
            }
        }
    }
}

fn cmd_timeline(project: Option<&str>, session_idx: usize) {
    let transcripts = find_transcripts(project);

    if transcripts.is_empty() {
        eprintln!("{RED}error:{RESET} No transcripts found. Run Claude Code first.");
        std::process::exit(1);
    }

    let path = if session_idx < transcripts.len() {
        &transcripts[session_idx]
    } else {
        eprintln!("{RED}error:{RESET} Session index {session_idx} out of range (have {})", transcripts.len());
        std::process::exit(1);
    };

    let (info, turns) = match parse_transcript(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{RED}error:{RESET} {e}");
            std::process::exit(1);
        }
    };

    // Header
    let project_name = info.project_dir.split('/').last().unwrap_or(&info.project_dir);
    let branch = info.git_branch.as_deref().unwrap_or("—");
    let tokens = format_tokens(info.total_input_tokens, info.total_output_tokens);

    println!();
    println!(
        "{BOLD}  Continuum{RESET}  {DIM}│{RESET}  {CYAN}{project_name}{RESET} {DIM}({branch}){RESET}  {DIM}│{RESET}  {tokens}"
    );
    println!("{DIM}  ─────────────────────────────────────────────────────{RESET}");
    println!();

    for turn in &turns {
        let time = time_from_iso(&turn.timestamp);
        let prompt_preview = if turn.prompt_text.len() > 70 {
            format!("{}…", &turn.prompt_text[..69])
        } else {
            turn.prompt_text.clone()
        };

        // Turn header
        println!(
            "  {BG_BLUE}{WHITE} {index} {RESET}  {DIM}{time}{RESET}  {BOLD}{prompt_preview}{RESET}",
            index = turn.index,
        );

        // Tool calls
        let tool_count = turn.tool_calls.len();
        for (i, tc) in turn.tool_calls.iter().enumerate() {
            let icon = tool_icon(&tc.name);
            let color = tool_color(&tc.name);
            let connector = if i == tool_count - 1 { "└─" } else { "├─" };
            let summary_display = if tc.summary.len() > 55 {
                format!("{}…", &tc.summary[..54])
            } else {
                tc.summary.clone()
            };

            println!(
                "        {DIM}{connector}{RESET} {icon} {color}{name}{RESET} {DIM}{summary}{RESET}",
                name = tc.name,
                summary = summary_display,
            );
        }

        // Summary line
        if tool_count > 0 || turn.total_output_tokens > 0 {
            let turn_tokens = format_tokens(turn.total_input_tokens, turn.total_output_tokens);
            let files_str = if turn.files_touched.is_empty() {
                String::new()
            } else {
                format!(
                    " · {} file{}",
                    turn.files_touched.len(),
                    if turn.files_touched.len() == 1 { "" } else { "s" }
                )
            };
            println!(
                "        {DIM}   {tool_count} tool call{s}{files_str} · {turn_tokens}{RESET}",
                s = if tool_count == 1 { "" } else { "s" },
            );
        }

        println!();
    }

    // Footer
    println!(
        "{DIM}  ─────────────────────────────────────────────────────{RESET}"
    );
    println!(
        "  {DIM}{count} checkpoints · Use the TUI for full navigation{RESET}",
        count = turns.len(),
    );
    println!();
}

fn cmd_peek(project: Option<&str>, turn_idx: usize) {
    let transcripts = find_transcripts(project);
    if transcripts.is_empty() {
        eprintln!("{RED}error:{RESET} No transcripts found.");
        std::process::exit(1);
    }

    let (info, turns) = match parse_transcript(&transcripts[0]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{RED}error:{RESET} {e}");
            std::process::exit(1);
        }
    };

    let turn = match turns.iter().find(|t| t.index == turn_idx) {
        Some(t) => t,
        None => {
            eprintln!("{RED}error:{RESET} Turn {turn_idx} not found (have {} turns)", turns.len());
            std::process::exit(1);
        }
    };

    let time = time_from_iso(&turn.timestamp);

    println!();
    println!("{DIM}┌──────────────────────────────────────────────────────────┐{RESET}");
    println!("{DIM}│{RESET} {BOLD}Turn {}{RESET}  {DIM}{time}{RESET}", turn.index);
    println!("{DIM}│{RESET}");
    println!("{DIM}│{RESET} {CYAN}Prompt:{RESET} {}", turn.prompt_text);
    println!("{DIM}│{RESET}");

    if !turn.tool_calls.is_empty() {
        println!("{DIM}│{RESET} {YELLOW}Tool calls ({}):{RESET}", turn.tool_calls.len());
        for tc in &turn.tool_calls {
            let icon = tool_icon(&tc.name);
            println!("{DIM}│{RESET}   {icon} {BOLD}{}{RESET}: {}", tc.name, tc.summary);
        }
        println!("{DIM}│{RESET}");
    }

    if !turn.files_touched.is_empty() {
        println!("{DIM}│{RESET} {GREEN}Files touched:{RESET}");
        for f in &turn.files_touched {
            println!("{DIM}│{RESET}   {GREEN}→{RESET} {f}");
        }
        println!("{DIM}│{RESET}");
    }

    if !turn.text_responses.is_empty() {
        println!("{DIM}│{RESET} {MAGENTA}Response preview:{RESET}");
        for resp in &turn.text_responses {
            let preview = if resp.len() > 200 {
                format!("{}…", &resp[..199])
            } else {
                resp.clone()
            };
            // Indent each line
            for line in preview.lines().take(8) {
                println!("{DIM}│{RESET}   {DIM}{line}{RESET}");
            }
        }
        println!("{DIM}│{RESET}");
    }

    let tokens = format_tokens(turn.total_input_tokens, turn.total_output_tokens);
    println!("{DIM}│{RESET} {DIM}Tokens: {tokens}{RESET}");
    println!("{DIM}└──────────────────────────────────────────────────────────┘{RESET}");
    println!();
}

// ── Main ────────────────────────────────────────────────────────────────────

fn print_help() {
    println!("{BOLD}continuum{RESET} — Claude Code conversation history navigator");
    println!();
    println!("{BOLD}USAGE{RESET}");
    println!("  continuum <command> [options]");
    println!();
    println!("{BOLD}COMMANDS{RESET}");
    println!("  {GREEN}sessions{RESET}                List Claude Code sessions for current project");
    println!("  {GREEN}timeline{RESET}  [--session N]  Show conversation timeline (default: latest)");
    println!("  {GREEN}peek{RESET}      <turn>         Show details of a specific turn/checkpoint");
    println!("  {GREEN}projects{RESET}                 List all projects with sessions");
    println!();
    println!("{BOLD}OPTIONS{RESET}");
    println!("  {DIM}--project <name>{RESET}   Target a specific project directory");
    println!("  {DIM}--session <N>{RESET}      Session index (0 = latest, default)");
    println!();
    println!("{BOLD}EXAMPLES{RESET}");
    println!("  {DIM}continuum timeline{RESET}              Show latest session's conversation");
    println!("  {DIM}continuum peek 3{RESET}                Peek at turn 3's details");
    println!("  {DIM}continuum sessions{RESET}              List sessions for current project");
    println!("  {DIM}continuum projects{RESET}              List all tracked projects");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        // Default: show timeline for latest session
        cmd_timeline(None, 0);
        return;
    }

    // Parse flags
    let mut project: Option<String> = None;
    let mut session_idx: usize = 0;
    let mut positional = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--project" if i + 1 < args.len() => {
                project = Some(args[i + 1].clone());
                i += 2;
            }
            "--session" if i + 1 < args.len() => {
                session_idx = args[i + 1].parse().unwrap_or(0);
                i += 2;
            }
            _ => {
                positional.push(args[i].as_str());
                i += 1;
            }
        }
    }

    let proj_ref = project.as_deref();

    match positional.first().copied() {
        Some("timeline" | "t" | "tl") => cmd_timeline(proj_ref, session_idx),
        Some("sessions" | "ls") => cmd_sessions(proj_ref),
        Some("projects" | "p") => {
            let projects = find_all_projects();
            if projects.is_empty() {
                println!("{DIM}No Claude Code projects found.{RESET}");
                return;
            }
            println!("{BOLD}Projects{RESET}");
            println!();
            for (name, _path, count) in &projects {
                let display = name.replace('-', "/");
                println!("  {GREEN}●{RESET} {BOLD}{display}{RESET}  {DIM}({count} sessions){RESET}");
            }
        }
        Some("peek") => {
            if let Some(idx_str) = positional.get(1) {
                let idx: usize = idx_str.parse().unwrap_or_else(|_| {
                    eprintln!("{RED}error:{RESET} '{idx_str}' is not a valid turn number");
                    std::process::exit(1);
                });
                cmd_peek(proj_ref, idx);
            } else {
                eprintln!("{RED}error:{RESET} peek requires a turn number. Use `continuum timeline` to see turns.");
            }
        }
        Some("help" | "--help" | "-h") => print_help(),
        Some(unknown) => {
            // Maybe they typed a number — treat as peek
            if let Ok(idx) = unknown.parse::<usize>() {
                cmd_peek(proj_ref, idx);
            } else {
                eprintln!("{RED}error:{RESET} unknown command: {unknown}");
                eprintln!("Run {BOLD}continuum help{RESET} for usage.");
            }
        }
        None => cmd_timeline(proj_ref, session_idx),
    }
}
