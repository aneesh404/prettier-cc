//! Minimap computation engine.
//!
//! Takes a session's command history and produces a list of `MinimapSegment`s
//! that fit within a given viewport height. Handles density encoding,
//! time-gap detection, and viewport positioning.

use crate::protocol::{CommandCategory, MinimapSegment, ViewportPosition};
use crate::session::SessionStore;

/// Maximum density value for normalization.
const MAX_OUTPUT_LINES: f32 = 200.0;

/// Minimum gap in seconds between commands to insert a time-gap segment.
const TIME_GAP_THRESHOLD_SECS: i64 = 60;

/// Braille density characters from empty to full (9 levels).
pub const BRAILLE_DENSITY: [char; 9] = [
    ' ', '⠁', '⠃', '⠇', '⡇', '⡏', '⡟', '⡿', '⣿',
];

/// Block density characters (4 levels).
pub const BLOCK_DENSITY: [char; 5] = [' ', '░', '▒', '▓', '█'];

pub struct MinimapBuilder<'a> {
    store: &'a SessionStore,
}

impl<'a> MinimapBuilder<'a> {
    pub fn new(store: &'a SessionStore) -> Self {
        Self { store }
    }

    /// Build minimap segments for a given TTY, fitting into `height` rows.
    pub fn build(
        &self,
        tty: &str,
        height: u32,
        viewport_offset: f32,
        viewport_size: f32,
    ) -> Option<(Vec<MinimapSegment>, ViewportPosition)> {
        let session = self.store.sessions.get(tty)?;
        if session.commands.is_empty() {
            return Some((vec![], ViewportPosition {
                offset: 0.0,
                size: 1.0,
            }));
        }

        let commands = &session.commands;
        let total_commands = commands.len();
        let height = height as usize;

        // If we have fewer commands than height, each command gets one row
        // plus potential time-gap rows. If more, we bucket.
        let mut segments = Vec::with_capacity(height);

        if total_commands <= height {
            // One segment per command, insert time gaps where appropriate
            for (i, cmd) in commands.iter().enumerate() {
                // Check for time gap with previous command
                if i > 0 {
                    if let (Some(prev_end), started) = (
                        commands[i - 1].ended_at,
                        cmd.started_at,
                    ) {
                        let gap = (started - prev_end).num_seconds();
                        if gap > TIME_GAP_THRESHOLD_SECS && segments.len() < height {
                            segments.push(MinimapSegment {
                                command_id: None,
                                label: None,
                                time: None,
                                density: 0.0,
                                is_error: false,
                                is_running: false,
                                category: CommandCategory::General,
                                in_viewport: false,
                            });
                        }
                    }
                }

                if segments.len() >= height {
                    break;
                }

                let density = cmd
                    .output_lines
                    .map(|l| (l as f32 / MAX_OUTPUT_LINES).min(1.0))
                    .unwrap_or(0.1);

                let is_error = cmd.exit_code.is_some_and(|c| c != 0);
                let is_running = cmd.ended_at.is_none();
                let time_str = cmd.started_at.format("%H:%M").to_string();
                let label = truncate_cmd(&cmd.cmd, 12);

                let progress = i as f32 / total_commands.max(1) as f32;
                let in_viewport = progress >= viewport_offset
                    && progress <= viewport_offset + viewport_size;

                segments.push(MinimapSegment {
                    command_id: Some(cmd.id.clone()),
                    label: Some(label),
                    time: Some(time_str),
                    density,
                    is_error,
                    is_running,
                    category: cmd.category.clone(),
                    in_viewport,
                });
            }
        } else {
            // Bucket commands into `height` segments
            let bucket_size = total_commands as f32 / height as f32;

            for row in 0..height {
                let start = (row as f32 * bucket_size) as usize;
                let end =
                    (((row + 1) as f32 * bucket_size) as usize).min(total_commands);
                let bucket = &commands[start..end];

                if bucket.is_empty() {
                    continue;
                }

                // Aggregate: max density, any errors, representative command
                let max_density = bucket
                    .iter()
                    .filter_map(|c| c.output_lines)
                    .max()
                    .map(|l| (l as f32 / MAX_OUTPUT_LINES).min(1.0))
                    .unwrap_or(0.1);

                let has_error = bucket
                    .iter()
                    .any(|c| c.exit_code.is_some_and(|code| code != 0));

                let has_running = bucket.iter().any(|c| c.ended_at.is_none());

                // Pick the most "interesting" command as representative
                let representative = bucket
                    .iter()
                    .find(|c| c.exit_code.is_some_and(|code| code != 0))
                    .or_else(|| {
                        bucket
                            .iter()
                            .find(|c| c.category != CommandCategory::General)
                    })
                    .unwrap_or(&bucket[0]);

                let time_str =
                    representative.started_at.format("%H:%M").to_string();
                let label = truncate_cmd(&representative.cmd, 12);

                let progress = (start as f32 + end as f32) / 2.0
                    / total_commands.max(1) as f32;
                let in_viewport = progress >= viewport_offset
                    && progress <= viewport_offset + viewport_size;

                // Pick the most specific category in the bucket
                let category = bucket
                    .iter()
                    .map(|c| &c.category)
                    .find(|c| **c != CommandCategory::General)
                    .cloned()
                    .unwrap_or(CommandCategory::General);

                segments.push(MinimapSegment {
                    command_id: Some(representative.id.clone()),
                    label: Some(label),
                    time: Some(time_str),
                    density: max_density,
                    is_error: has_error,
                    is_running: has_running,
                    category,
                    in_viewport,
                });
            }
        }

        let viewport = ViewportPosition {
            offset: viewport_offset,
            size: viewport_size,
        };

        Some((segments, viewport))
    }

    /// Render a minimap to a vector of styled lines (for TUI or debug output).
    pub fn render_ascii(
        &self,
        tty: &str,
        height: u32,
    ) -> Option<Vec<String>> {
        let (segments, _vp) = self.build(tty, height, 0.8, 0.2)?;
        let mut lines = Vec::with_capacity(segments.len() + 2);

        lines.push("╭─ CONTINUUM ─────╮".into());

        for seg in &segments {
            if seg.command_id.is_none() {
                // Time gap
                lines.push("│  ┆           │".into());
                continue;
            }

            let marker = if seg.is_running {
                "⋯"
            } else if seg.is_error {
                "✗"
            } else {
                "✓"
            };

            let color_hint = if seg.is_error {
                "!" // red
            } else if seg.is_running {
                "~" // yellow
            } else {
                " " // green
            };

            let time = seg.time.as_deref().unwrap_or("     ");
            let label = seg.label.as_deref().unwrap_or("");

            // Command marker line
            lines.push(format!(
                "│ {time}  ●{color_hint}{marker} │"
            ));

            // Density bar
            let bar = density_to_braille_bar(seg.density, 8);
            let vp_marker = if seg.in_viewport { "▶" } else { "┃" };
            lines.push(format!("│  {vp_marker} {bar:<8} │"));

            // Label line (only if there's room)
            if !label.is_empty() {
                lines.push(format!("│    {label:<10} │"));
            }
        }

        lines.push("╰───────────────╯".into());
        Some(lines)
    }
}

/// Truncate a command string for display in the rail.
fn truncate_cmd(cmd: &str, max_len: usize) -> String {
    let cmd = cmd.trim();
    if cmd.len() <= max_len {
        cmd.to_string()
    } else {
        format!("{}…", &cmd[..max_len - 1])
    }
}

/// Convert a density value (0.0..1.0) to a braille bar string of given width.
pub fn density_to_braille_bar(density: f32, width: usize) -> String {
    let filled = (density * width as f32).ceil() as usize;
    let filled = filled.min(width);

    let level = (density * (BRAILLE_DENSITY.len() - 1) as f32).round() as usize;
    let level = level.min(BRAILLE_DENSITY.len() - 1);
    let ch = BRAILLE_DENSITY[level];

    let mut bar = String::with_capacity(width);
    for i in 0..width {
        if i < filled {
            bar.push(ch);
        } else {
            bar.push(' ');
        }
    }
    bar
}

/// Convert density to a single block character.
pub fn density_to_block(density: f32) -> char {
    let idx = (density * (BLOCK_DENSITY.len() - 1) as f32).round() as usize;
    BLOCK_DENSITY[idx.min(BLOCK_DENSITY.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate() {
        assert_eq!(truncate_cmd("ls -la", 12), "ls -la");
        assert_eq!(truncate_cmd("npm run build:production:release", 12), "npm run bui…");
    }

    #[test]
    fn test_density_bar() {
        let bar = density_to_braille_bar(0.0, 8);
        assert_eq!(bar.trim(), "");

        let bar = density_to_braille_bar(1.0, 8);
        assert_eq!(bar.chars().count(), 8);
        assert!(bar.chars().all(|c| c == '⣿'));
    }

    #[test]
    fn test_density_block() {
        assert_eq!(density_to_block(0.0), ' ');
        assert_eq!(density_to_block(1.0), '█');
        assert_eq!(density_to_block(0.5), '▒');
    }
}
