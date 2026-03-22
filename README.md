# Rewind Rail

**Navigate your Claude Code conversations like a timeline. Rewind, replay, and understand every AI-assisted coding session.**

Rewind Rail is a terminal-native tool that parses Claude Code's conversation transcripts and presents them as a navigable timeline of checkpoints. Each checkpoint captures the user prompt, the AI response, every tool call made (file edits, bash commands, searches), token usage, and files touched — giving you full visibility into long, complex AI coding sessions.

---

## The Problem

When working with Claude Code on large tasks — refactoring a codebase, debugging a production issue, building a new feature — sessions can grow to dozens or hundreds of turns. You lose track of:

- **What happened when?** — Which turn introduced a bug, refactored that module, or ran those tests?
- **What tools were used?** — Did Claude edit the file or just read it? What bash commands ran?
- **How much context was consumed?** — Token usage across a session tells you when Claude is working with stale context.
- **Where to resume?** — After stepping away, you need to find the right checkpoint to continue from.

Claude Code stores conversation history as `.jsonl` transcript files, but they're raw JSON — thousands of lines of streaming message fragments, tool invocations, and metadata. Rewind Rail transforms this into something human-readable and navigable.

## How It Works

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│   ~/.claude/projects/         Rewind Rail parses these      │
│   ├── project-a/              transcript files and builds   │
│   │   ├── session1.jsonl  ──► a structured timeline of      │
│   │   └── session2.jsonl      turns, tool calls, and        │
│   └── project-b/              token usage for each session  │
│       └── session3.jsonl                                    │
│                                                             │
│   Shell Hooks (optional)      The daemon watches for new    │
│   ├── rewind.zsh          ──► commands in your terminal     │
│   ├── rewind.bash             and indexes them alongside    │
│   └── rewind.fish             conversation history          │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

Rewind Rail consists of three components:

| Component | Binary | Role |
|-----------|--------|------|
| **CLI** | `rewind` | Quick terminal commands: list projects, view timelines, peek at turns |
| **TUI** | `rewind-tui` | Interactive terminal UI with project browser, session split view, and embedded Claude Code |
| **Daemon** | `rewindd` | Background service that watches transcripts and ingests shell events |

---

## Screenshots

### Projects Screen

Browse all your Claude Code projects, sorted by most recent activity:

```
┌──────────────────────────────────────────────────────────────────┐
│ ⏪ Rewind Rail │ 28 projects                                     │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│ ▸  Documents/git/repos/arcx/orderbook                            │
│       32 sessions · 2h ago                                       │
│                                                                  │
│    Documents/git/repos/argocd                                    │
│       14 sessions · 5h ago                                       │
│                                                                  │
│    Documents/scratchpad                                          │
│       7 sessions · 1d ago                                        │
│                                                                  │
│    Documents/git/repos/paradex/cloud/observability/stack         │
│       9 sessions · 2d ago                                        │
│                                                                  │
│    Documents/git/repos/karnot/madara/operator                    │
│       8 sessions · 3d ago                                        │
│                                                                  │
├──────────────────────────────────────────────────────────────────┤
│ ↑↓ navigate  Enter open  q quit                                  │
└──────────────────────────────────────────────────────────────────┘
```

### Session Split View

Select a project to see all sessions on the left and the conversation timeline on the right:

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ Rewind Rail │ Documents/git/repos/arcx/orderbook │ 32 sessions              │
├─────────────────────┬───────────────────────────────┬────────────────────────┤
│  Sessions           │  Timeline (main) · 536k↓ 2k↑ │  Details               │
│                     │                               │                        │
│ ▸ ● 0311e06d  HEAD  │  ▶  1  10:04  Use this git…  │  Turn 12               │
│     12 prompts      │     7 tool calls · 280k↓ 1k↑ │  ──────────            │
│     48 tools        │                               │  Fix the orderbook     │
│     2026-03-22      │  ▶  2  10:15  Fix the test…   │  matching engine to    │
│                     │     3 tool calls · 45k↓ 800↑  │  handle partial fills  │
│   ○ 34cd156b        │                               │  correctly when…       │
│     8 prompts       │  ▼  3  10:22  Refactor the…   │                        │
│     31 tools        │     ├─ 📖 Read  src/engine.rs │  Tools: 5              │
│     2026-03-21      │     ├─ ✏️  Edit  src/engine.rs │  Files: 3              │
│                     │     ├─ 💻 Bash  cargo test     │  Tokens: 52k↓ 1.2k↑   │
│   ○ a8f29c01        │     ├─ ✏️  Edit  src/types.rs  │                        │
│     5 prompts       │     └─ 💻 Bash  cargo build    │  Top tools:            │
│     19 tools        │       5 tool calls · 3 files  │  Bash (24) Edit (18)   │
│     2026-03-20      │                               │  Read (12) Grep (8)    │
│                     │  ▶  4  10:31  Now run the…    │                        │
│                     │     2 tool calls · 31k↓ 600↑  │                        │
├─────────────────────┴───────────────────────────────┴────────────────────────┤
│ ↑↓ navigate  Tab switch pane  Space expand  Enter fullscreen  a agent  q quit│
└──────────────────────────────────────────────────────────────────────────────┘
```

### CLI Timeline

Quick timeline view directly in your terminal:

```
  ⏪ Rewind Rail  │  orderbook (main)  │  536.5k↓ 2.1k↑
  ─────────────────────────────────────────────────────

   1   10:04  Use this git repo to push the content of this…
        ├─ 💻 Bash  ls -la /Users/aneesh/Documents/arcx/order…
        ├─ 📝 Write  orderbook/.gitignore
        ├─ 💻 Bash  git add .gitignore Cargo.lock Cargo.toml…
        └─ 💻 Bash  git push -u origin main
           4 tool calls · 1 file · 280.7k↓ 1.2k↑

   2   10:15  Fix the failing test in the matching engine…
        ├─ 📖 Read  src/engine.rs
        ├─ ✏️  Edit  src/engine.rs
        └─ 💻 Bash  cargo test
           3 tool calls · 1 file · 45.2k↓ 800↑

   3   10:22  Refactor the order types to support partial fills…
        ├─ 📖 Read  src/types.rs
        ├─ ✏️  Edit  src/types.rs
        ├─ ✏️  Edit  src/engine.rs
        ├─ 💻 Bash  cargo test
        └─ 💻 Bash  cargo build
           5 tool calls · 3 files · 52.1k↓ 1.2k↑

  ─────────────────────────────────────────────────────
  3 checkpoints · Use Esc+Esc in Claude Code to rewind
```

---

## Features

### Timeline Navigation
Every conversation turn is a **checkpoint** showing: the user prompt, timestamp, all tool calls with icons and summaries, token usage (input/output), and files touched. Expand any turn to see the full tool call tree.

### Multi-Session Browser
Browse sessions across all your Claude Code projects. Sessions are sorted by recency with metadata like prompt count, tool call count, and date — so you can find exactly the session you're looking for.

### Embedded Claude Code
Launch Claude Code directly inside the TUI. Resume a previous session or fork it to explore an alternative approach — all without leaving Rewind Rail.

### Smart Transcript Parsing
The transcript parser handles Claude Code's streaming `.jsonl` format: deduplicates streamed message fragments by UUID, extracts turns with proper grouping, and summarizes tool calls (file paths are shortened, bash commands are truncated, agent tasks are labeled).

### Shell Integration
Optional shell hooks for Bash, Zsh, and Fish send command execution events (start, end, exit code) to the daemon over a Unix socket. This indexes your terminal commands alongside AI conversation history.

### Minimap Visualization
The daemon builds a density-encoded minimap of your command history using Braille characters (`⠁⠃⠇⡇⡏⡟⡿⣿`), with gap detection for idle periods and viewport tracking.

### Live Reload
The TUI watches transcript files for changes using `notify` and auto-refreshes when Claude Code writes new conversation turns — so the timeline updates in real-time as you work.

---

## Installation

### Prerequisites
- **Rust** (1.70+) — [Install via rustup](https://rustup.rs/)
- **Claude Code** — Rewind Rail reads Claude Code's transcript files from `~/.claude/projects/`

### Quick Install

```bash
git clone https://github.com/aneesh404/prettier-cc.git
cd prettier-cc
./install.sh
```

This will:
1. Build all binaries (`rewind`, `rewind-tui`, `rewindd`) in release mode
2. Install them to `~/.local/bin/` (or `/usr/local/bin` as fallback)
3. Copy shell hooks to `~/.config/rewind/`
4. Auto-detect your shell and add the source line to your RC file
5. Set up the daemon as a launchd service (macOS) or systemd service (Linux)

### TUI Only

If you just want the interactive TUI and CLI:

```bash
./install-tui.sh
```

### Manual Build

```bash
cargo build --release
# Binaries are in target/release/
./target/release/rewind --help
./target/release/rewind-tui
```

---

## Usage

### CLI

```bash
# List all projects with Claude Code sessions
rewind projects

# Show timeline for the current project (latest session)
rewind timeline

# Show timeline for a specific project
rewind timeline --project /path/to/project

# Show a specific session (0 = latest, 1 = second latest, etc.)
rewind timeline --session 1

# Peek at a specific turn with full details
rewind peek 3

# List sessions for a project
rewind sessions
```

### TUI

```bash
# Launch the interactive TUI
rewind-tui

# Open a specific transcript file directly
rewind-tui /path/to/transcript.jsonl
```

**Keybindings:**

| Key | Action |
|-----|--------|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Enter` | Select / open / fullscreen |
| `Space` | Expand/collapse turn |
| `Tab` | Switch pane (split view) |
| `e` / `c` | Expand all / collapse all |
| `a` | Launch embedded Claude Code agent |
| `r` | Reload timeline |
| `p` | Back to projects |
| `Esc` | Back |
| `q` | Quit |

---

## Architecture

```
rewind-rail/
├── crates/
│   ├── rewind-cli/          # CLI binary — quick terminal commands
│   │   └── src/main.rs
│   ├── rewind-daemon/       # Core library + daemon binary
│   │   └── src/
│   │       ├── config.rs        # TOML config loader
│   │       ├── transcript.rs    # .jsonl transcript parser
│   │       ├── session.rs       # Command indexing & categorization
│   │       ├── minimap.rs       # Braille density visualization
│   │       ├── persistence.rs   # Append-only session storage
│   │       ├── listener.rs      # Unix socket I/O
│   │       └── protocol.rs      # Shared data types
│   └── rewind-tui/          # Interactive terminal UI
│       └── src/
│           ├── main.rs          # ratatui-based UI (projects, split, timeline)
│           ├── embedded.rs      # PTY-based embedded Claude Code
│           └── chat.rs          # Interactive chat (WIP)
├── shell/
│   ├── rewind.bash          # Bash integration (DEBUG trap + PROMPT_COMMAND)
│   ├── rewind.zsh           # Zsh integration (preexec/precmd hooks)
│   └── rewind.fish          # Fish integration (event system)
├── config/
│   └── config.toml          # Default daemon configuration
├── install.sh               # Full installer (build + shell + service)
└── install-tui.sh           # TUI-only installer
```

### Tech Stack

- **Rust** with Cargo workspace
- **[ratatui](https://github.com/ratatui/ratatui)** — Terminal UI framework
- **[crossterm](https://github.com/crossterm-rs/crossterm)** — Cross-platform terminal handling
- **[tokio](https://tokio.rs)** — Async runtime for the daemon
- **[notify](https://github.com/notify-rs/notify)** — File watcher for live reload
- **[portable-pty](https://docs.rs/portable-pty)** + **[vt100](https://docs.rs/vt100)** — Embedded terminal emulation

---

## Configuration

The daemon reads `~/.config/rewind/config.toml`:

```toml
data_dir = "~/.rewind"
ingest_socket = "~/.rewind/ingest.sock"
query_socket = "~/.rewind/query.sock"
max_session_age_days = 7
rail_side = "right"
rail_width = 16
hover_zone_px = 8
animation_ms = 200
debug = false
```

---

## Contributing

Contributions are welcome. Here's how to get started:

### Setting Up for Development

```bash
git clone https://github.com/aneesh404/prettier-cc.git
cd prettier-cc
cargo build
cargo test
```

### Areas to Contribute

- **Daemon completion** — The daemon (`rewindd`) has the architecture in place but needs the main event loop wired up to serve queries over the Unix socket.
- **Chat interface** — `crates/rewind-tui/src/chat.rs` is scaffolded for an interactive chat mode inside the TUI.
- **Search & filter** — Add the ability to search turns by prompt text, tool name, or file path.
- **Diff view** — Show file diffs for each turn (the transcript contains file snapshots).
- **Export** — Export a session timeline to markdown or HTML for sharing.
- **More shell support** — Improve output line estimation in the bash/zsh hooks.
- **Theme support** — The TUI uses hardcoded colors; make them configurable.

### Submitting Changes

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Make your changes
4. Run `cargo build && cargo test` to verify
5. Commit with a clear message explaining the *why*
6. Open a pull request

---

## License

MIT
