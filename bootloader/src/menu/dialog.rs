use alloc::{string::String, vec::Vec};
use core::{cmp, fmt::Write};

use crate::graphics::{
    canvas::{Color, SimpleCanvas},
    console::Console,
};

const CHAR_WIDTH: usize = 8;
const CHAR_HEIGHT: usize = 16;

#[derive(Clone, Copy)]
pub enum DialogType {
    Info,
    Warning,
    Error,
}

pub struct Dialog {
    pub title: String,
    pub content: String,
    pub typ: DialogType,
}

impl Dialog {
    pub fn new(title: &str, content: &str, typ: DialogType) -> Self {
        Self {
            title: String::from(title),
            content: String::from(content),
            typ,
        }
    }

    pub fn show(&self, console: &mut Console) {
        let mode = *console.canvas().get_mode();
        let screen_width = mode.width as usize;
        let screen_height = mode.height as usize;

        let padding = 10;
        let border_thickness = 2;
        let title_height = 30; // Space for title

        // Max dimensions
        let max_w = screen_width / 3;
        let max_h = screen_height / 2; // Approximate menu height limit

        // Calculate content width/height
        let max_content_width = max_w.saturating_sub(padding * 2 + border_thickness * 2);
        let max_chars_per_line = max_content_width / CHAR_WIDTH;

        let mut wrapped_lines = Vec::new();
        if !self.content.is_empty() {
            for line in self.content.lines() {
                let chars: Vec<char> = line.chars().collect();
                if chars.len() <= max_chars_per_line {
                    wrapped_lines.push(String::from(line));
                } else {
                    for chunk in chars.chunks(max_chars_per_line) {
                        wrapped_lines.push(chunk.iter().collect::<String>());
                    }
                }
            }
        }

        let content_lines_count = wrapped_lines.len();
        let content_height = content_lines_count * CHAR_HEIGHT;

        let mut box_w = self.title.len() * CHAR_WIDTH + padding * 2;
        if !wrapped_lines.is_empty() {
            let max_line_len = wrapped_lines.iter().map(|l| l.len()).max().unwrap_or(0);
            box_w = cmp::max(box_w, max_line_len * CHAR_WIDTH + padding * 2);
        }

        // Clamp width
        box_w = cmp::min(box_w, max_w);
        box_w = cmp::max(box_w, 200); // Minimum width

        // Calculate height
        let mut box_h = title_height + padding * 2;
        if !self.content.is_empty() {
            box_h += content_height + 10; // 10 for spacing between title and content
        }

        // Clamp height
        box_h = cmp::min(box_h, max_h);

        let box_x = (screen_width - box_w) / 2;
        let box_y = (screen_height - box_h) / 2;

        // Draw Background
        console.fill_rect(box_x, box_y, box_w, box_h, Color::BLACK);

        let border_color = match self.typ {
            DialogType::Info => Color::WHITE,
            DialogType::Warning => Color::YELLOW,
            DialogType::Error => Color::RED,
        };

        console.draw_box(box_x, box_y, box_w, box_h, border_color);

        // Draw Title
        console.set_cursor(box_x + padding, box_y + padding);

        let title_color_code = match self.typ {
            DialogType::Info => "37",    // White
            DialogType::Warning => "33", // Yellow
            DialogType::Error => "31",   // Red
        };

        // Use ANSI to set color. Note: Console expects \x1b[3Xm
        // We use \x1b[37m to reset to white at the end, just in case.
        // We format the string first to ensure the escape sequence is passed as a whole
        // to write_str
        let title_str = format!("\x1b[{}m{}\x1b[37m", title_color_code, self.title);
        let _ = console.write_str(&title_str);

        // Draw Content
        if !self.content.is_empty() {
            let content_start_y = box_y + padding + title_height;

            for (i, line) in wrapped_lines.iter().enumerate() {
                let line_y = content_start_y + i * CHAR_HEIGHT;
                if line_y + CHAR_HEIGHT > box_y + box_h - padding {
                    break; // Clip if too long
                }
                console.set_cursor(box_x + padding, line_y);
                // Ensure white color for content
                let _ = write!(console, "\x1b[37m{}", line);
            }
        }

        console.commit();
    }
}
