//! Embedded terminal — runs Claude Code in a PTY and renders it inside the TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

pub struct EmbeddedTerm {
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pub rows: u16,
    pub cols: u16,
    pub exited: bool,
}

impl EmbeddedTerm {
    /// Spawn a command in a PTY with an optional working directory.
    pub fn spawn(cmd: &str, args: &[&str], rows: u16, cols: u16, cwd: Option<&std::path::Path>) -> Result<Self, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to open PTY: {e}"))?;

        let mut cmd_builder = CommandBuilder::new(cmd);
        for arg in args {
            cmd_builder.arg(*arg);
        }
        // Inherit environment
        for (key, val) in std::env::vars() {
            cmd_builder.env(key, val);
        }
        // Set working directory if provided
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

        // Drop the slave side — the child owns it now
        drop(pair.slave);

        // Background thread to read PTY output
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

        let parser = vt100::Parser::new(rows, cols, 1000); // 1000 lines scrollback

        Ok(EmbeddedTerm {
            parser,
            writer,
            output_rx: rx,
            child,
            rows,
            cols,
            exited: false,
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

    /// Resize the PTY.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(rows, cols);
        // Note: portable-pty doesn't have a direct resize on the child,
        // but the parser resize should handle display correctly.
    }

    /// Get the current screen state for rendering.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Render the terminal screen into ratatui Lines.
    pub fn render_lines(&self, height: u16, width: u16) -> Vec<Line<'static>> {
        let screen = self.parser.screen();
        let mut lines = Vec::with_capacity(height as usize);

        for row in 0..height {
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

                        let mut style = Style::default().fg(fg).bg(bg);
                        if cell.bold() {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if cell.italic() {
                            style = style.add_modifier(Modifier::ITALIC);
                        }
                        if cell.underline() {
                            style = style.add_modifier(Modifier::UNDERLINED);
                        }
                        if cell.inverse() {
                            // Swap fg/bg
                            style = Style::default().fg(bg).bg(fg);
                            if cell.bold() {
                                style = style.add_modifier(Modifier::BOLD);
                            }
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

    match key.code {
        KeyCode::Char(c) if ctrl => {
            // Ctrl+A = 0x01, Ctrl+B = 0x02, ..., Ctrl+Z = 0x1A
            let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
            if byte <= 26 {
                Some(vec![byte])
            } else {
                Some(vec![c as u8])
            }
        }
        KeyCode::Char(c) if alt => {
            let mut bytes = vec![0x1b]; // ESC prefix for Alt
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
