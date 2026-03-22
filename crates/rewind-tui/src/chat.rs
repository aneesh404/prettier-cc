//! Headless Claude Code runner — spawns `claude -p` and streams output.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

pub struct ChatMessage {
    pub role: Role,
    pub text: String,
}

#[derive(Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

pub struct ChatSession {
    pub session_id: String,
    pub project_dir: PathBuf,
    pub messages: Vec<ChatMessage>,
    pub input: String,
    pub cursor_pos: usize,
    pub streaming: bool,
    pub scroll_offset: u16,
    output_rx: Option<mpsc::Receiver<OutputChunk>>,
    child: Option<Child>,
    pub forked: bool,
}

enum OutputChunk {
    Line(String),
    Done,
    Error(String),
}

impl ChatSession {
    pub fn new(session_id: String, project_dir: PathBuf) -> Self {
        ChatSession {
            session_id,
            project_dir,
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            streaming: false,
            scroll_offset: 0,
            output_rx: None,
            child: None,
            forked: false,
        }
    }

    /// Send a prompt to Claude Code in headless mode.
    pub fn send_prompt(&mut self, fork: bool) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        // Add user message
        self.messages.push(ChatMessage {
            role: Role::User,
            text: prompt.clone(),
        });

        // Clear input
        self.input.clear();
        self.cursor_pos = 0;

        // Build command
        let mut cmd = Command::new("claude");
        cmd.arg("-p").arg(&prompt);
        cmd.arg("--resume").arg(&self.session_id);

        if fork && !self.forked {
            cmd.arg("--fork-session");
            self.forked = true;
        }

        // Set CWD to the project directory so claude can find the session
        if self.project_dir.exists() {
            cmd.current_dir(&self.project_dir);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());

        match cmd.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();
                let (tx, rx) = mpsc::channel();

                // Stream stdout in background
                let tx_out = tx.clone();
                thread::spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        match line {
                            Ok(l) => {
                                if tx_out.send(OutputChunk::Line(l)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = tx_out.send(OutputChunk::Done);
                });

                // Stream stderr in background
                let tx_err = tx;
                thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines() {
                        match line {
                            Ok(l) if !l.trim().is_empty() => {
                                let _ = tx_err.send(OutputChunk::Error(l));
                            }
                            _ => {}
                        }
                    }
                });

                self.child = Some(child);
                self.output_rx = Some(rx);
                self.streaming = true;

                // Add empty assistant message that we'll stream into
                self.messages.push(ChatMessage {
                    role: Role::Assistant,
                    text: String::new(),
                });
            }
            Err(e) => {
                self.messages.push(ChatMessage {
                    role: Role::Assistant,
                    text: format!("Error starting claude: {e}\nMake sure 'claude' is on your PATH."),
                });
            }
        }
    }

    /// Poll for new output chunks. Call this every frame.
    pub fn poll_output(&mut self) {
        if let Some(ref rx) = self.output_rx {
            loop {
                match rx.try_recv() {
                    Ok(OutputChunk::Line(line)) => {
                        if let Some(msg) = self.messages.last_mut() {
                            if msg.role == Role::Assistant {
                                if !msg.text.is_empty() {
                                    msg.text.push('\n');
                                }
                                msg.text.push_str(&line);
                            }
                        }
                    }
                    Ok(OutputChunk::Error(line)) => {
                        if let Some(msg) = self.messages.last_mut() {
                            if msg.role == Role::Assistant {
                                if !msg.text.is_empty() {
                                    msg.text.push('\n');
                                }
                                msg.text.push_str(&format!("[stderr] {line}"));
                            }
                        }
                    }
                    Ok(OutputChunk::Done) => {
                        self.streaming = false;
                        self.output_rx = None;
                        // Wait for child
                        if let Some(mut child) = self.child.take() {
                            let _ = child.wait();
                        }
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.streaming = false;
                        self.output_rx = None;
                        break;
                    }
                }
            }
        }
    }

    // ── Input editing ───────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.cursor_pos -= prev;
            self.input.remove(self.cursor_pos);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.input.remove(self.cursor_pos);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.cursor_pos -= prev;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            let next = self.input[self.cursor_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.cursor_pos += next;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_pos = self.input.len();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
    }
}
