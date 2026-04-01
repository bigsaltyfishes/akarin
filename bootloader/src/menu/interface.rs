use alloc::{string::String, vec::Vec};
use core::fmt::Write;

use uefi::proto::console::{serial::Serial, text::Key};

use crate::{
    config::ast::{Menu as AstMenu, MenuItem},
    graphics::{canvas::Color, console::Console},
    menu::{
        dialog::{Dialog, DialogType},
        io_event::WaitEvent,
    },
};

pub trait MenuInterface {
    fn init(&mut self);
    fn draw_menu(&mut self, menu: &AstMenu, parents: &[AstMenu], selected_index: usize);
    fn update_selection(
        &mut self,
        menu: &AstMenu,
        parents: &[AstMenu],
        old_index: usize,
        new_index: usize,
    );
    fn show_dialog(&mut self, title: &str, content: &str, typ: DialogType);
    fn clear(&mut self);
    fn read_key(&mut self) -> Option<Key>;
    fn wait_for_key(&mut self) -> Key;
    fn wait_for_key_or_timeout(&mut self, timeout_ms: u64) -> Option<Key>;
    fn write_str(&mut self, s: &str);
    fn update_timeout(&mut self, timeout_remaining: u64);
}

pub struct GraphicalInterface<'a> {
    console: &'a mut Console,
    box_x: usize,
    box_y: usize,
    box_w: usize,
    box_h: usize,
}

impl<'a> GraphicalInterface<'a> {
    pub fn new(console: &'a mut Console) -> Self {
        Self {
            console,
            box_x: 0,
            box_y: 0,
            box_w: 0,
            box_h: 0,
        }
    }

    fn draw_item(&mut self, text: &str, is_selected: bool, index: usize) {
        let x = self.box_x + 10;
        let y = self.box_y + 10 + index * 20;

        self.console.set_cursor(x, y);
        if is_selected {
            let width = text.len() * 8 + 16;
            self.console.fill_rect(x, y, width, 16, Color::WHITE);
            self.console.draw_char(' ', Color::BLACK, Color::WHITE);
            for c in text.chars() {
                self.console.draw_char(c, Color::BLACK, Color::WHITE);
            }
            self.console.draw_char(' ', Color::BLACK, Color::WHITE);
        } else {
            let width = text.len() * 8 + 16;
            self.console.fill_rect(x, y, width, 16, Color::BLACK);

            let _ = write!(self.console, "  {}", text);
        }
    }

    fn draw_item_at_index(
        &mut self,
        menu: &AstMenu,
        parents: &[AstMenu],
        index: usize,
        is_selected: bool,
    ) {
        let has_parent = !parents.is_empty();
        let items_offset = if has_parent { 1 } else { 0 };

        if has_parent && index == 0 {
            self.draw_item("..", is_selected, 0);
            return;
        }

        let item_index = index - items_offset;
        let visible_items: Vec<&MenuItem> = menu
            .items
            .iter()
            .filter(|i| matches!(i, MenuItem::Entry(_) | MenuItem::Menu(_)))
            .collect();

        if let Some(item) = visible_items.get(item_index) {
            match item {
                MenuItem::Entry(e) => self.draw_item(&e.name, is_selected, index),
                MenuItem::Menu(m) => {
                    let name = m.name.clone() + "/";
                    self.draw_item(&name, is_selected, index);
                }
                _ => {}
            }
        }
    }
}

impl<'a> MenuInterface for GraphicalInterface<'a> {
    fn init(&mut self) {
        self.console.enable_double_buffering();
        self.console.clear();
        self.console.commit();
    }

    fn draw_menu(&mut self, menu: &AstMenu, parents: &[AstMenu], selected_index: usize) {
        self.console.clear();

        let mode = *self.console.canvas().get_mode();
        let width = mode.width as usize;
        let height = mode.height as usize;

        self.box_x = 50;
        self.box_y = 50;
        self.box_w = width - 100;
        self.box_h = height - 100;

        // Draw title centered
        let title = "Bootloader Version 0.1.0";
        let title_len = title.len() * 8;
        let title_x = (width - title_len) / 2;
        self.console.set_cursor(title_x, 10);
        let _ = writeln!(self.console, "{}", title);

        // Draw box
        self.console
            .draw_box(self.box_x, self.box_y, self.box_w, self.box_h, Color::WHITE);

        // Draw all items
        let has_parent = !parents.is_empty();
        let items_offset = if has_parent { 1 } else { 0 };

        if has_parent {
            let is_selected = selected_index == 0;
            self.draw_item("..", is_selected, 0);
        }

        let visible_items: Vec<&MenuItem> = menu
            .items
            .iter()
            .filter(|i| matches!(i, MenuItem::Entry(_) | MenuItem::Menu(_)))
            .collect();

        for (i, item) in visible_items.iter().enumerate() {
            let index = i + items_offset;
            let is_selected = index == selected_index;

            match item {
                MenuItem::Entry(e) => self.draw_item(&e.name, is_selected, index),
                MenuItem::Menu(m) => {
                    let name = m.name.clone() + "/";
                    self.draw_item(&name, is_selected, index);
                }
                _ => {}
            }
        }

        self.console.commit();
    }

    fn update_selection(
        &mut self,
        menu: &AstMenu,
        parents: &[AstMenu],
        old_index: usize,
        new_index: usize,
    ) {
        if old_index == new_index {
            return;
        }

        // Redraw old item as unselected
        self.draw_item_at_index(menu, parents, old_index, false);

        // Redraw new item as selected
        self.draw_item_at_index(menu, parents, new_index, true);

        self.console.commit();
    }

    fn show_dialog(&mut self, title: &str, content: &str, typ: DialogType) {
        let dialog = Dialog::new(title, content, typ);
        dialog.show(self.console);
        self.console.commit();
    }

    fn clear(&mut self) {
        self.console.clear();
        self.console.commit();
    }

    fn read_key(&mut self) -> Option<Key> {
        WaitEvent::Keypress(None).poll()
    }

    fn wait_for_key(&mut self) -> Key {
        WaitEvent::Keypress(None).wait().unwrap()
    }

    fn wait_for_key_or_timeout(&mut self, timeout_ms: u64) -> Option<Key> {
        WaitEvent::KeypressOrTimeout {
            keypress: None,
            timeout: timeout_ms,
        }
        .wait()
    }

    fn write_str(&mut self, s: &str) {
        let _ = self.console.write_str(s);
        self.console.commit();
    }

    fn update_timeout(&mut self, timeout_remaining: u64) {
        let seconds = (timeout_remaining + 999) / 1000;
        let msg = alloc::format!("Booting in {}s... ", seconds);

        let (old_x, old_y) = self.console.get_cursor();
        let width = self.console.canvas().get_mode().width as usize;
        let height = self.console.canvas().get_mode().height as usize;

        // Position at bottom center
        // Assuming char width ~8 (INPUT_CHAR_WIDTH) and height 16 (CHAR_HEIGHT)
        let char_width = 8;
        let char_height = 16;
        let text_width = msg.len() * char_width;

        let x = if width > text_width {
            (width - text_width) / 2
        } else {
            0
        };
        let y = if height > char_height + 20 {
            height - char_height - 20
        } else {
            0
        };

        self.console.set_cursor(x, y);
        // Draw with default color (White)
        let _ = self.console.write_str(&msg);

        self.console.set_cursor(old_x, old_y);
        self.console.commit();
    }
}

pub struct SerialInterface {
    menu_end_row: usize,
}

impl SerialInterface {
    pub fn new() -> Self {
        Self { menu_end_row: 1 }
    }

    fn write_bytes(&mut self, s: &[u8]) {
        if let Ok(handle) = uefi::boot::get_handle_for_protocol::<Serial>() {
            if let Ok(mut serial) = uefi::boot::open_protocol_exclusive::<Serial>(handle) {
                let _ = serial.write(s);
            }
        }
    }
}

impl MenuInterface for SerialInterface {
    fn init(&mut self) {
        self.clear();
    }

    fn draw_menu(&mut self, menu: &AstMenu, parents: &[AstMenu], selected_index: usize) {
        self.clear();
        self.write_str("\x1b[H");

        self.write_str("Bootloader Version 0.1.0\r\n");
        self.write_str("========================\r\n\r\n");

        if !parents.is_empty() {
            if selected_index == 0 {
                self.write_str("\x1b[7m .. \x1b[0m\r\n");
            } else {
                self.write_str(" .. \r\n");
            }
        }

        let has_parent = !parents.is_empty();
        let items_offset = if has_parent { 1 } else { 0 };

        let visible_items: Vec<&MenuItem> = menu
            .items
            .iter()
            .filter(|i| matches!(i, MenuItem::Entry(_) | MenuItem::Menu(_)))
            .collect();

        for (i, item) in visible_items.iter().enumerate() {
            let index = i + items_offset;
            let is_selected = index == selected_index;

            let name = match item {
                MenuItem::Entry(e) => e.name.clone(),
                MenuItem::Menu(m) => m.name.clone() + "/",
                _ => String::new(),
            };

            if name.is_empty() {
                continue;
            }

            if is_selected {
                self.write_str("\x1b[7m ");
                self.write_str(&name);
                self.write_str(" \x1b[0m\r\n");
            } else {
                self.write_str(" ");
                self.write_str(&name);
                self.write_str(" \r\n");
            }
        }

        // Calculate end row: 3 (header) + (1 if parent) + items count
        self.menu_end_row = 3 + (if has_parent { 1 } else { 0 }) + visible_items.len();
    }

    fn update_selection(
        &mut self,
        menu: &AstMenu,
        parents: &[AstMenu],
        _old_index: usize,
        new_index: usize,
    ) {
        self.draw_menu(menu, parents, new_index);
    }

    fn show_dialog(&mut self, title: &str, content: &str, typ: DialogType) {
        self.clear();
        let color = match typ {
            DialogType::Info => "\x1b[37m",
            DialogType::Warning => "\x1b[33m",
            DialogType::Error => "\x1b[31m",
        };

        self.write_str(color);
        self.write_str(title);
        self.write_str("\x1b[0m\r\n");
        self.write_str("----------------\r\n");
        self.write_str(content);
        self.write_str("\r\n\r\nPress any key to continue...");
    }

    fn clear(&mut self) {
        self.write_str("\x1b[2J\x1b[H");
    }

    fn read_key(&mut self) -> Option<Key> {
        WaitEvent::Keypress(None).poll()
    }

    fn wait_for_key(&mut self) -> Key {
        WaitEvent::Keypress(None).wait().unwrap()
    }

    fn wait_for_key_or_timeout(&mut self, timeout_ms: u64) -> Option<Key> {
        WaitEvent::KeypressOrTimeout {
            keypress: None,
            timeout: timeout_ms,
        }
        .wait()
    }

    fn write_str(&mut self, s: &str) {
        self.write_bytes(s.as_bytes());
    }

    fn update_timeout(&mut self, timeout_remaining: u64) {
        let seconds = (timeout_remaining + 999) / 1000;
        let mut msg = String::new();
        // Use DECSC (Save Cursor) \x1b7 and DECRC (Restore Cursor) \x1b8 which are
        // safer Move to Row menu_end_row + 2 (leave one empty line), Col 1
        let row = self.menu_end_row + 2;
        let _ = write!(msg, "\x1b7\x1b[{};1HBooting in {}s... \x1b8", row, seconds);
        self.write_str(&msg);
    }
}

pub struct CompositeInterface<'a> {
    graphical: Option<GraphicalInterface<'a>>,
    serial: Option<SerialInterface>,
}

impl<'a> CompositeInterface<'a> {
    pub fn new(graphical: Option<GraphicalInterface<'a>>, serial: Option<SerialInterface>) -> Self {
        Self {
            graphical,
            #[cfg(feature = "logger_debug")]
            serial,
            #[cfg(not(feature = "logger_debug"))]
            serial: None,
        }
    }
}

impl<'a> MenuInterface for CompositeInterface<'a> {
    fn init(&mut self) {
        if let Some(g) = &mut self.graphical {
            g.init();
        }
        if let Some(s) = &mut self.serial {
            s.init();
        }
    }

    fn draw_menu(&mut self, menu: &AstMenu, parents: &[AstMenu], selected_index: usize) {
        if let Some(g) = &mut self.graphical {
            g.draw_menu(menu, parents, selected_index);
        }
        if let Some(s) = &mut self.serial {
            s.draw_menu(menu, parents, selected_index);
        }
    }

    fn update_selection(
        &mut self,
        menu: &AstMenu,
        parents: &[AstMenu],
        old_index: usize,
        new_index: usize,
    ) {
        if let Some(g) = &mut self.graphical {
            g.update_selection(menu, parents, old_index, new_index);
        }
        if let Some(s) = &mut self.serial {
            s.update_selection(menu, parents, old_index, new_index);
        }
    }

    fn show_dialog(&mut self, title: &str, content: &str, typ: DialogType) {
        if let Some(g) = &mut self.graphical {
            g.show_dialog(title, content, typ);
        }
        if let Some(s) = &mut self.serial {
            s.show_dialog(title, content, typ);
        }
    }

    fn clear(&mut self) {
        if let Some(g) = &mut self.graphical {
            g.clear();
        }
        if let Some(s) = &mut self.serial {
            s.clear();
        }
    }

    fn read_key(&mut self) -> Option<Key> {
        if let Some(g) = &mut self.graphical {
            if let Some(k) = g.read_key() {
                return Some(k);
            }
        }
        if let Some(s) = &mut self.serial {
            if let Some(k) = s.read_key() {
                return Some(k);
            }
        }
        None
    }

    fn wait_for_key(&mut self) -> Key {
        WaitEvent::Keypress(None).wait().unwrap()
    }

    fn wait_for_key_or_timeout(&mut self, timeout_ms: u64) -> Option<Key> {
        WaitEvent::KeypressOrTimeout {
            keypress: None,
            timeout: timeout_ms,
        }
        .wait()
    }

    fn write_str(&mut self, s: &str) {
        if let Some(g) = &mut self.graphical {
            g.write_str(s);
        }
        if let Some(serial_iface) = &mut self.serial {
            serial_iface.write_str(s);
        }
    }

    fn update_timeout(&mut self, timeout_remaining: u64) {
        if let Some(g) = &mut self.graphical {
            g.update_timeout(timeout_remaining);
        }
        if let Some(s) = &mut self.serial {
            s.update_timeout(timeout_remaining);
        }
    }
}
