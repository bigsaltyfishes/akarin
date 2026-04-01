use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use uefi::proto::console::text::Key;

use crate::{
    config::parser::{Lexer, TokenKind},
    menu::{dialog::DialogType, interface::MenuInterface},
    session::Session,
};

pub enum VTAction {
    Continue,
    Exit,
}

pub struct VirtualTerminal<'a, 'b> {
    session: Session<'a>,
    interface: &'b mut dyn MenuInterface,
}

impl<'a, 'b> VirtualTerminal<'a, 'b> {
    pub fn new(session: Session<'a>, interface: &'b mut dyn MenuInterface) -> Self {
        Self { session, interface }
    }

    pub fn run(mut self) -> (Session<'a>, Option<()>) {
        self.interface.clear();
        self.interface.write_str("Hikari Bootloader VT\r\n");
        self.interface
            .write_str("Commands: kernel, initramfs, set, boot, exit\r\n");

        loop {
            self.interface.write_str("VT> ");
            let line = self.read_line();
            if line.trim().is_empty() {
                continue;
            }

            match self.process_command(&line) {
                VTAction::Exit => return (self.session, None),
                VTAction::Continue => {}
            }
        }
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        loop {
            // Use interface.wait_for_key()
            let key = self.interface.wait_for_key();
            match key {
                Key::Printable(c) => {
                    let c: char = char::from(c);
                    if c == '\r' {
                        self.interface.write_str("\r\n");
                        return line;
                    } else if c == '\x08' {
                        // Backspace
                        if !line.is_empty() {
                            line.pop();
                            // Send backspace, space, backspace to erase
                            self.interface.write_str("\x08 \x08");
                        }
                    } else {
                        let s = format!("{}", c);
                        self.interface.write_str(&s);
                        line.push(c);
                    }
                }
                _ => {}
            }
        }
    }

    fn process_command(&mut self, line: &str) -> VTAction {
        let mut lexer = Lexer::new(line);
        let mut tokens = Vec::new();
        while let Some(token) = lexer.next_token() {
            tokens.push(token);
        }

        if tokens.is_empty() {
            return VTAction::Continue;
        }

        match &tokens[0].kind {
            TokenKind::Keyword(k) | TokenKind::Identifier(k) => match k.as_str() {
                "exit" | "quit" => return VTAction::Exit,
                "boot" => {
                    // Show Loading Dialog
                    self.interface
                        .show_dialog("Booting", "Loading Kernel...", DialogType::Info);

                    match self.session.boot() {
                        Ok(_) => {
                            // Should not return
                        }
                        Err(e) => {
                            let msg = format!("Boot Error:\n{}", e);
                            self.interface
                                .show_dialog("Boot Error", &msg, DialogType::Error);
                            self.interface.wait_for_key();

                            self.interface.clear();
                            self.interface.write_str("Hikari Bootloader VT\r\n");
                        }
                    }
                }
                "set" => {
                    // set key = value
                    if tokens.len() >= 4 {
                        if let TokenKind::Identifier(key) = &tokens[1].kind {
                            if let TokenKind::Equals = &tokens[2].kind {
                                let val_token = &tokens[3];
                                let val = match &val_token.kind {
                                    TokenKind::String(s) => s.clone(),
                                    TokenKind::Identifier(s) => s.clone(),
                                    TokenKind::Number(n) => n.to_string(),
                                    _ => String::new(),
                                };
                                // Eval value?
                                let eval_val = self.session.evaluator.eval_string(&val);
                                self.session.evaluator.set(key.clone(), eval_val);
                            }
                        }
                    } else {
                        self.interface.write_str("Usage: set key = value\r\n");
                    }
                }
                "kernel" => {
                    // kernel path arg1 arg2 ...
                    if tokens.len() < 2 {
                        self.interface.write_str("Usage: kernel path [args...]\r\n");
                        return VTAction::Continue;
                    }

                    let path_token = &tokens[1];
                    let path = match &path_token.kind {
                        TokenKind::String(s) => self.session.evaluator.eval_string(s),
                        TokenKind::Identifier(s) => s.clone(), // Literal
                        _ => String::new(),
                    };

                    let mut args = Vec::new();

                    for token in tokens.iter().skip(2) {
                        match &token.kind {
                            TokenKind::String(s) => {
                                args.push(self.session.evaluator.eval_string(s))
                            }
                            TokenKind::Identifier(s) => args.push(s.clone()),
                            TokenKind::Keyword(s) => args.push(s.clone()),
                            TokenKind::Number(n) => args.push(n.to_string()),
                            _ => {}
                        }
                    }
                    self.session.set_kernel(path, args);
                }
                "bootstrap" => {
                    if tokens.len() != 2 {
                        self.interface.write_str("Usage: bootstrap path\r\n");
                        return VTAction::Continue;
                    }

                    let path = match &tokens[1].kind {
                        TokenKind::String(s) => self.session.evaluator.eval_string(s),
                        TokenKind::Identifier(s) => s.clone(),
                        TokenKind::Keyword(s) => s.clone(),
                        _ => String::new(),
                    };
                    self.session.set_bootstrap(path);
                }
                "initramfs" => {
                    self.interface.write_str("initramfs not implemented\r\n");
                }
                _ => {
                    let msg = format!("Unknown command: {}\r\n", k);
                    self.interface.write_str(&msg);
                }
            },
            _ => self.interface.write_str("Invalid command\r\n"),
        }
        VTAction::Continue
    }
}
