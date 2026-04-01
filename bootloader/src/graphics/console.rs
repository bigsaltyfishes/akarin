use core::fmt;

use crate::{
    graphics::{
        canvas::{Color, SimpleCanvas},
        screen::Screen,
    },
    resources::{UefiResource, framebuffer::Framebuffer},
};

static mut CONSOLE: Option<Console> = None;

pub const BACKUP_CHAR: char = '�';
pub const CHAR_HEIGHT: usize = 16;
pub const INPUT_CHAR_WIDTH: usize = 8;

#[derive(Debug)]
pub struct Console {
    screen: Screen,
    line_spacing: usize,
    letter_spacing: usize,
    border_padding: usize,
    x_pos: usize,
    y_pos: usize,
}

impl Console {
    pub fn init() -> Option<Self> {
        let framebuffer = Framebuffer::resource()?;
        let mut screen = Screen::new(framebuffer);
        screen.clear();
        screen.enable_double_buffering();
        Some(Self {
            screen,
            line_spacing: 2,
            letter_spacing: 0,
            border_padding: 1,
            x_pos: 1,
            y_pos: 1,
        })
    }

    pub fn set_line_spacing(&mut self, spacing: usize) {
        self.line_spacing = spacing;
    }

    pub fn set_letter_spacing(&mut self, spacing: usize) {
        self.letter_spacing = spacing;
    }

    pub fn set_border_padding(&mut self, padding: usize) {
        self.border_padding = padding;
    }

    pub fn set_cursor(&mut self, x: usize, y: usize) {
        self.x_pos = x;
        self.y_pos = y;
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.x_pos, self.y_pos)
    }
}

impl Console {
    pub fn enable_double_buffering(&mut self) {
        self.screen.enable_double_buffering();
    }

    pub fn clear(&mut self) {
        self.screen.clear();
        self.y_pos = self.border_padding;
        self.x_pos = self.border_padding;
    }

    pub fn new_line(&mut self) {
        self.x_pos = self.border_padding;
        self.y_pos += self.line_spacing + CHAR_HEIGHT;
    }

    pub fn move_up(&mut self) {
        let screen = &mut self.screen;
        let dy = self.y_pos + self.line_spacing + CHAR_HEIGHT - screen.get_mode().height as usize;
        self.y_pos -= dy;
        screen.move_up(dy as u32);
    }

    pub fn draw_char(&mut self, c: char, fg: Color, bg: Color) {
        if c == '\n' {
            self.new_line();
        } else if c == '\r' {
            self.x_pos = self.border_padding;
        } else if c == '\x08' {
            if self.x_pos >= self.border_padding + INPUT_CHAR_WIDTH {
                self.x_pos -= INPUT_CHAR_WIDTH;
            }
        } else {
            let glyph = unifont::get_glyph(c).unwrap_or(unifont::get_glyph(BACKUP_CHAR).unwrap());
            if self.x_pos + glyph.get_width() >= self.screen.get_mode().width as usize {
                self.new_line();
            }
            if self.y_pos + CHAR_HEIGHT + self.line_spacing > self.screen.get_mode().height as usize
            {
                self.move_up();
            }

            for y in 0..CHAR_HEIGHT {
                for x in 0..glyph.get_width() {
                    if glyph.get_pixel(x, y) {
                        self.screen.draw_pixel(
                            (self.x_pos + x) as u32,
                            (self.y_pos + y) as u32,
                            &fg,
                        );
                    } else {
                        self.screen.draw_pixel(
                            (self.x_pos + x) as u32,
                            (self.y_pos + y) as u32,
                            &bg,
                        );
                    }
                }
            }
            self.x_pos += glyph.get_width() + self.letter_spacing;
        }
    }

    pub fn draw_box(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        // Top
        self.fill_rect(x, y, w, 2, color);
        // Bottom
        self.fill_rect(x, y + h - 2, w, 2, color);
        // Left
        self.fill_rect(x, y, 2, h, color);
        // Right
        self.fill_rect(x + w - 2, y, 2, h, color);
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        for cy in y..y + h {
            for cx in x..x + w {
                self.screen.draw_pixel(cx as u32, cy as u32, &color);
            }
        }
    }

    /// Get a mutable reference to the underlying canvas (Screen).
    pub fn canvas(&mut self) -> &mut Screen {
        &mut self.screen
    }

    pub fn commit(&mut self) {
        self.screen.commit();
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut chars: core::str::Chars<'_> = s.chars();
        let mut current_color = Color::WHITE;

        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.next() == Some('[') {
                if let (Some(color_code_one), Some(color_code_two), Some('m')) =
                    (chars.next(), chars.next(), chars.next())
                {
                    if let (Some(3), Some(code)) =
                        (color_code_one.to_digit(10), color_code_two.to_digit(10))
                    {
                        current_color = Color::from_ansi_code(code as u8);
                        continue;
                    }
                }
            }
            self.draw_char(ch, current_color, Color::BLACK);
        }
        Ok(())
    }
}

pub fn console_instance() -> Option<&'static mut Console> {
    unsafe {
        let console = &raw mut CONSOLE;
        (*console).as_mut()
    }
}

pub fn enable_console() {
    unsafe {
        CONSOLE = Console::init();
    }
}
