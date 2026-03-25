//! Embedded terminal — runs Claude Code in a PTY and renders it inside the TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

/// How many times larger the PTY is than the visible viewport.
const SCROLL_MULTIPLIER: u16 = 5;

pub struct EmbeddedTerm {
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Total rows the PTY/parser uses (visible_rows * SCROLL_MULTIPLIER).
    pub pty_rows: u16,
    /// The number of rows the user actually sees in the viewport.
    pub visible_rows: u16,
    pub cols: u16,
    pub exited: bool,
    /// Viewport scroll offset: 0 = bottom (live), >0 = scrolled up by N rows.
    pub viewport_offset: u16,
}

impl EmbeddedTerm {
    /// Spawn a command in a PTY with an optional working directory.
    /// The PTY is created with `visible_rows * SCROLL_MULTIPLIER` rows so that
    /// the child app renders more content, and we show a scrollable viewport.
    pub fn spawn(cmd: &str, args: &[&str], visible_rows: u16, cols: u16, cwd: Option<&std::path::Path>) -> Result<Self, String> {
        let pty_rows = visible_rows.saturating_mul(SCROLL_MULTIPLIER).max(visible_rows);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: pty_rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to open PTY: {e}"))?;

        let mut cmd_builder = CommandBuilder::new(cmd);
        for arg in args {
            cmd_builder.arg(*arg);
        }
        for (key, val) in std::env::vars() {
            cmd_builder.env(key, val);
        }
        if let Some(dir) = cwd {
            if dir.exists() {
                cmd_builder.cwd(dir);
            }
        }

        let child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| format!("Failed to spawn {cmd}: {e}"))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("Failed to take PTY writer: {e}"))?;

        drop(pair.slave);

        let (tx, rx) = mpsc::channel();
        let mut reader = reader;
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let parser = vt100::Parser::new(pty_rows, cols, 0);

        Ok(EmbeddedTerm {
            parser,
            writer,
            output_rx: rx,
            child,
            pty_rows,
            visible_rows,
            cols,
            exited: false,
            viewport_offset: 0,
        })
    }

    /// Process any pending output from the PTY.
    pub fn process_output(&mut self) {
        while let Ok(data) = self.output_rx.try_recv() {
            self.parser.process(&data);
        }
    }

    /// Send raw bytes to the PTY (keyboard input).
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Send a crossterm KeyEvent to the PTY.
    pub fn send_key(&mut self, key: KeyEvent) {
        if let Some(bytes) = key_to_bytes(key) {
            self.send_bytes(&bytes);
        }
    }

    /// Check if the child process has exited.
    pub fn is_running(&mut self) -> bool {
        if self.exited {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                false
            }
            _ => true,
        }
    }

    /// Scroll the viewport up (toward earlier content) by N rows.
    pub fn scroll_up(&mut self, n: u16) {
        let max_offset = self.pty_rows.saturating_sub(self.visible_rows);
        self.viewport_offset = (self.viewport_offset + n).min(max_offset);
    }

    /// Scroll the viewport down (toward live/bottom) by N rows.
    pub fn scroll_down(&mut self, n: u16) {
        self.viewport_offset = self.viewport_offset.saturating_sub(n);
    }

    /// Snap viewport to the bottom (live view).
    pub fn snap_to_bottom(&mut self) {
        self.viewport_offset = 0;
    }

    /// Returns true if the viewport is scrolled up from the bottom.
    pub fn is_scrolled(&self) -> bool {
        self.viewport_offset > 0
    }

    /// Render the terminal screen into ratatui Lines.
    /// Shows a viewport window into the oversized PTY screen.
    /// When viewport_offset=0, shows the bottom rows (where input/status bar live).
    /// When scrolled up, shows earlier rows.
    pub fn render_lines(&self, height: u16, width: u16) -> Vec<Line<'static>> {
        let screen = self.parser.screen();
        let mut lines = Vec::with_capacity(height as usize);

        // Compute which row range to render:
        // offset=0 → show the bottom `height` rows of the PTY
        // offset=N → shift the window up by N rows
        let bottom_start = self.pty_rows.saturating_sub(height);
        let start_row = bottom_start.saturating_sub(self.viewport_offset);

        for row in start_row..(start_row + height) {
            if row >= self.pty_rows {
                lines.push(Line::from(""));
                continue;
            }
            let mut spans = Vec::new();
            let mut col = 0u16;

            while col < width {
                let cell = screen.cell(row, col);
                match cell {
                    Some(cell) => {
                        let contents = cell.contents();
                        let ch = if contents.is_empty() {
                            " ".to_string()
                        } else {
                            contents.to_string()
                        };

                        let fg = vt100_color_to_ratatui(cell.fgcolor());
                        let bg = vt100_color_to_ratatui(cell.bgcolor());

                        let mut style = if cell.inverse() {
                            Style::default().fg(bg).bg(fg)
                        } else {
                            Style::default().fg(fg).bg(bg)
                        };

                        if cell.bold() {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if cell.italic() {
                            style = style.add_modifier(Modifier::ITALIC);
                        }
                        if cell.underline() {
                            style = style.add_modifier(Modifier::UNDERLINED);
                        }

                        spans.push(Span::styled(ch, style));
                        col += 1;
                    }
                    None => {
                        spans.push(Span::raw(" "));
                        col += 1;
                    }
                }
            }

            lines.push(Line::from(spans));
        }

        lines
    }
}

/// Convert a vt100 color to a ratatui color.
fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Convert a crossterm KeyEvent to the bytes a terminal process expects.
fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Enter if shift => Some(b"\x1b[13;2u".to_vec()),
        KeyCode::Char(c) if ctrl => {
            let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
            if byte <= 26 {
                Some(vec![byte])
            } else {
                Some(vec![c as u8])
            }
        }
        KeyCode::Char(c) if alt => {
            let mut bytes = vec![0x1b];
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            Some(bytes)
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}
