pub mod dialog;
pub mod interface;
pub mod io_event;
pub mod terminal;

use alloc::{string::String, vec::Vec};

use uefi::proto::console::text::{Key, ScanCode};

use crate::{
    config::ast::{Config, EntryStatement, Menu as AstMenu, MenuItem},
    fs::{FileSystem, path::Path, simple::SimpleFileSystem},
    graphics::console::Console,
    menu::interface::{CompositeInterface, GraphicalInterface, MenuInterface, SerialInterface},
    session::Session,
};

pub struct Menu<'a> {
    session: Option<Session<'a>>,
    config: Config,
    interface: CompositeInterface<'a>,
}

impl<'a> Menu<'a> {
    pub fn new(fs: &'a mut SimpleFileSystem, console: Option<&'a mut Console>) -> Self {
        let evaluator = crate::config::eval::Evaluator::new();
        let mut session = Session::new(fs, evaluator);

        let graphical = console.map(|c| GraphicalInterface::new(c));
        let serial = SerialInterface::new();
        let mut interface = CompositeInterface::new(graphical, Some(serial));

        // Read & Parse Config
        let config_path = Path::try_from("/boot.cfg").expect("Invalid config path");
        let content = session
            .fs
            .read(config_path)
            .expect("Failed to read config file");
        let string_content = String::from_utf8(content).expect("Invalid UTF-8 in config");
        let mut parser = crate::config::parser::Parser::new(&string_content);
        let (config, warnings) = match parser.parse() {
            Ok((c, w)) => (c, w),
            Err(e) => {
                let msg = format!("Error at Line {}, Col {}:\n{}", e.line, e.col, e.reason);
                interface.show_dialog("Configuration Error", &msg, dialog::DialogType::Error);
                loop {
                    if let Some(_key) = interface.wait_for_key_or_timeout(100) {
                        break;
                    }
                }
                (
                    Config {
                        timeout: None,
                        default: None,
                        root_items: Vec::new(),
                    },
                    Vec::new(),
                )
            }
        };

        if !warnings.is_empty() {
            let mut warning_msg = String::new();
            for w in warnings {
                let msg = format!("Warning at Line {}, Col {}:\n{}", w.line, w.col, w.reason);
                warning_msg.push_str(&msg);
            }
            interface.show_dialog(
                "Configuration Warning",
                &warning_msg,
                dialog::DialogType::Warning,
            );
            interface.wait_for_key();
        }

        // Load global vars
        for item in &config.root_items {
            if let MenuItem::Set(k, v) = item {
                session
                    .evaluator
                    .set(k.clone(), session.evaluator.eval_string(v));
            }
        }
        session.evaluator.push_scope();

        Self {
            session: Some(session),
            config,
            interface,
        }
    }

    pub fn run(&mut self) {
        // Disable logger
        self.interface.init();

        let root_menu = AstMenu {
            name: String::from("Main Menu"),
            items: self.config.root_items.clone(),
        };

        let mut current_menu = root_menu;
        let mut parent_stack: Vec<AstMenu> = Vec::new();

        // Find default index
        let mut selected_index = 0;
        if let Some(default_name) = &self.config.default {
            for (i, item) in current_menu.items.iter().enumerate() {
                if let MenuItem::Entry(e) = item {
                    if &e.name == default_name {
                        selected_index = i;
                        break;
                    }
                }
            }
        }

        // Validate timeout
        if self.config.timeout.is_some() && self.config.default.is_none() {
            self.interface.show_dialog(
                "Configuration Warning",
                "Timeout ignored because no default entry is set",
                dialog::DialogType::Warning,
            );
            self.interface.wait_for_key();
            self.config.timeout = None;
        }

        let mut timeout_remaining = self.config.timeout.unwrap_or(0);
        let mut needs_redraw = true;
        let mut last_selected_index = selected_index;

        loop {
            if self.config.root_items.is_empty() {
                // No menu items, launch terminal
                if let Some(session) = self.session.take() {
                    let vt = terminal::VirtualTerminal::new(session, &mut self.interface);
                    let (session, _) = vt.run();
                    self.session = Some(session);
                    continue;
                }
            } else if needs_redraw {
                self.interface
                    .draw_menu(&current_menu, &parent_stack, selected_index);
                needs_redraw = false;
                last_selected_index = selected_index;
            } else if last_selected_index != selected_index {
                self.interface.update_selection(
                    &current_menu,
                    &parent_stack,
                    last_selected_index,
                    selected_index,
                );
                last_selected_index = selected_index;
            }

            // Handle Timeout
            let key = if timeout_remaining > 0 {
                self.interface.update_timeout(timeout_remaining);
                match self.interface.wait_for_key_or_timeout(1000) {
                    Some(k) => {
                        timeout_remaining = 0;
                        needs_redraw = true;
                        Some(k)
                    }
                    None => {
                        if timeout_remaining >= 1000 {
                            timeout_remaining -= 1000;
                        } else {
                            timeout_remaining = 0;
                        }

                        if timeout_remaining == 0 {
                            Some(Key::Printable(uefi::Char16::try_from('\r').unwrap()))
                        } else {
                            None
                        }
                    }
                }
            } else {
                Some(self.interface.wait_for_key())
            };

            let has_parent = !parent_stack.is_empty();
            let items_offset = if has_parent { 1 } else { 0 };

            let visible_items: Vec<&MenuItem> = current_menu
                .items
                .iter()
                .filter(|i| matches!(i, MenuItem::Entry(_) | MenuItem::Menu(_)))
                .collect();
            let total_items = visible_items.len() + items_offset;

            match key {
                Some(Key::Special(ScanCode::UP)) => {
                    if selected_index > 0 {
                        selected_index -= 1;
                    }
                }
                Some(Key::Special(ScanCode::DOWN)) => {
                    if selected_index < total_items - 1 {
                        selected_index += 1;
                    }
                }
                Some(Key::Printable(c)) if c == 'c' => {
                    if let Some(session) = self.session.take() {
                        let vt = terminal::VirtualTerminal::new(session, &mut self.interface);
                        let (session, _) = vt.run();
                        self.session = Some(session);
                    }
                    needs_redraw = true;
                }
                Some(Key::Printable(c)) if c == '\r' => {
                    if has_parent && selected_index == 0 {
                        if let Some(parent) = parent_stack.pop() {
                            current_menu = parent;
                            selected_index = 0;
                            needs_redraw = true;
                        }
                    } else {
                        let item_index = selected_index - items_offset;
                        match &visible_items[item_index] {
                            MenuItem::Entry(e) => {
                                if let Some(session) = &mut self.session {
                                    session.kernel_path = None;
                                    session.kernel_args.clear();
                                    session.bootstrap_path = None;

                                    for stmt in &e.statements {
                                        match stmt {
                                            EntryStatement::Set(k, v) => {
                                                let val = session.evaluator.eval_string(v);
                                                session.evaluator.set(k.clone(), val);
                                            }
                                            EntryStatement::Kernel { path, args } => {
                                                let k_path = session.evaluator.eval_string(path);
                                                let k_args: Vec<String> = args
                                                    .iter()
                                                    .map(|a| match a {
                                                        crate::config::ast::Arg::Literal(s) => {
                                                            session.evaluator.eval_string(s)
                                                        }
                                                        crate::config::ast::Arg::Expression(s) => {
                                                            session.evaluator.eval_string(s)
                                                        }
                                                    })
                                                    .collect();
                                                session.set_kernel(k_path, k_args);
                                            }
                                            EntryStatement::Bootstrap { path } => {
                                                let bootstrap_path =
                                                    session.evaluator.eval_string(path);
                                                session.set_bootstrap(bootstrap_path);
                                            }
                                            _ => {}
                                        }
                                    }

                                    self.interface.show_dialog(
                                        "Booting",
                                        "Loading Kernel...",
                                        dialog::DialogType::Info,
                                    );

                                    match session.boot() {
                                        Err(err) => {
                                            let msg = format!("{}", err);
                                            self.interface.show_dialog(
                                                "Boot Error",
                                                &msg,
                                                dialog::DialogType::Error,
                                            );
                                            self.interface.wait_for_key();
                                            needs_redraw = true;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            MenuItem::Menu(m) => {
                                parent_stack.push(current_menu.clone());
                                current_menu = m.clone();
                                selected_index = 0;
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
