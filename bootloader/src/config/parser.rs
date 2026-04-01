use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{iter::Peekable, str::Chars};

use thiserror::Error;

use super::ast::*;

#[derive(Debug, Error, Clone)]
#[error("Line {line}, Col {col}: {reason}")]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub reason: String,
}

#[derive(Debug, Error, Clone)]
#[error("Line {line}, Col {col}: {reason}")]
pub struct ParseWarning {
    pub line: usize,
    pub col: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Keyword(String),
    String(String),
    Number(u64),
    Identifier(String),
    LBrace,
    RBrace,
    Equals,
    Semicolon,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub col: usize,
}

pub struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            chars: input.chars().peekable(),
            line: 1,
            col: 1,
        }
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() {
                self.next_char();
            } else {
                break;
            }
        }
    }

    fn skip_comment(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c == '\n' {
                break;
            }
            self.next_char();
        }
    }

    fn read_string(&mut self) -> Option<TokenKind> {
        self.next_char(); // consume "
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c == '"' {
                self.next_char();
                return Some(TokenKind::String(s));
            }
            s.push(c);
            self.next_char();
        }
        None // Unterminated string
    }

    fn read_number(&mut self) -> Option<TokenKind> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.next_char();
            } else {
                break;
            }
        }

        let mut unit = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_alphabetic() {
                unit.push(c);
                self.next_char();
            } else {
                break;
            }
        }

        let val = s.parse::<u64>().ok()?;
        let ms = match unit.as_str() {
            "s" => val * 1000,
            "ms" => val,
            "" => val,
            _ => return None, // Invalid unit
        };
        Some(TokenKind::Number(ms))
    }

    fn read_identifier(&mut self) -> Option<TokenKind> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '/' || c == '.' || c == '-' {
                s.push(c);
                self.next_char();
            } else {
                break;
            }
        }

        match s.as_str() {
            "timeout" | "entry" | "menu" | "default" | "kernel" | "bootstrap" | "initramfs"
            | "set" => Some(TokenKind::Keyword(s)),
            _ => Some(TokenKind::Identifier(s)),
        }
    }

    pub fn next_token(&mut self) -> Option<Token> {
        self.skip_whitespace();

        let start_line = self.line;
        let start_col = self.col;

        let c = *self.chars.peek()?;
        let kind = match c {
            '{' => {
                self.next_char();
                Some(TokenKind::LBrace)
            }
            '}' => {
                self.next_char();
                Some(TokenKind::RBrace)
            }
            '=' => {
                self.next_char();
                Some(TokenKind::Equals)
            }
            ';' => {
                self.next_char();
                Some(TokenKind::Semicolon)
            }
            '"' => self.read_string(),
            '/' => {
                self.next_char();
                if let Some('/') = self.chars.peek() {
                    self.next_char();
                    self.skip_comment();
                    return self.next_token();
                } else {
                    self.read_identifier_starting_with('/')
                }
            }
            c if c.is_ascii_digit() => self.read_number(),
            c if is_ident_char(c) => self.read_identifier(),
            _ => {
                self.next_char();
                None
            }
        };

        kind.map(|k| Token {
            kind: k,
            line: start_line,
            col: start_col,
        })
    }

    fn read_identifier_starting_with(&mut self, start: char) -> Option<TokenKind> {
        let mut s = String::new();
        s.push(start);
        while let Some(&c) = self.chars.peek() {
            if c.is_alphanumeric()
                || c == '_'
                || c == '/'
                || c == '.'
                || c == '-'
                || c == '{'
                || c == '}'
            {
                s.push(c);
                self.next_char();
            } else {
                break;
            }
        }
        Some(TokenKind::Identifier(s))
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-'
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(input: &str) -> Self {
        let mut lexer = Lexer::new(input);
        let mut tokens = Vec::new();
        while let Some(token) = lexer.next_token() {
            tokens.push(token);
        }
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
        self.pos += 1;
        t
    }

    fn error(&self, reason: String) -> ParseError {
        if let Some(token) = self.peek() {
            ParseError {
                line: token.line,
                col: token.col,
                reason,
            }
        } else if let Some(last) = self.tokens.last() {
            ParseError {
                line: last.line,
                col: last.col + 1, // Approximate
                reason: format!("Unexpected EOF: {}", reason),
            }
        } else {
            ParseError {
                line: 1,
                col: 1,
                reason: format!("Empty file: {}", reason),
            }
        }
    }

    fn warning(&self, reason: String) -> ParseWarning {
        if let Some(token) = self.peek() {
            ParseWarning {
                line: token.line,
                col: token.col,
                reason,
            }
        } else if let Some(last) = self.tokens.last() {
            ParseWarning {
                line: last.line,
                col: last.col + 1,
                reason,
            }
        } else {
            ParseWarning {
                line: 1,
                col: 1,
                reason,
            }
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        match self.advance() {
            Some(Token {
                kind: TokenKind::Keyword(k),
                ..
            }) if k == kw => Ok(()),
            _ => Err(self.error(format!("Expected keyword '{}'", kw))),
        }
    }

    fn expect_string(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Some(Token {
                kind: TokenKind::String(s),
                ..
            }) => Ok(s.clone()),
            _ => Err(self.error("Expected string".to_string())),
        }
    }

    fn expect_identifier_or_string(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Some(Token {
                kind: TokenKind::String(s),
                ..
            }) => Ok(s.clone()),
            Some(Token {
                kind: TokenKind::Identifier(s),
                ..
            }) => Ok(s.clone()),
            Some(Token {
                kind: TokenKind::Keyword(s),
                ..
            }) => Ok(s.clone()),
            _ => Err(self.error("Expected identifier or string".to_string())),
        }
    }

    pub fn parse(&mut self) -> Result<(Config, Vec<ParseWarning>), ParseError> {
        let mut config = Config {
            timeout: None,
            default: None,
            root_items: Vec::new(),
        };
        let mut warnings = Vec::new();
        let mut defined_names = Vec::new();

        while self.peek().is_some() {
            match self.peek() {
                Some(Token {
                    kind: TokenKind::Keyword(k),
                    ..
                }) => match k.as_str() {
                    "timeout" => {
                        if config.timeout.is_some() {
                            warnings
                                .push(self.warning("Timeout defined multiple times".to_string()));
                        }
                        self.advance();
                        match self.advance() {
                            Some(Token {
                                kind: TokenKind::Number(n),
                                ..
                            }) => config.timeout = Some(*n),
                            _ => {
                                return Err(self.error("Expected number after timeout".to_string()));
                            }
                        }
                        self.expect_semicolon()?;
                    }
                    "default" => {
                        if config.default.is_some() {
                            warnings
                                .push(self.warning("Default defined multiple times".to_string()));
                        }
                        self.advance(); // consume default
                        // Check if next is entry
                        if let Some(Token {
                            kind: TokenKind::Keyword(k),
                            ..
                        }) = self.peek()
                        {
                            if k == "entry" {
                                let (entry, entry_warnings) = self.parse_entry()?;
                                warnings.extend(entry_warnings);
                                if defined_names.contains(&entry.name) {
                                    return Err(
                                        self.error(format!("Duplicate entry name: {}", entry.name))
                                    );
                                }
                                defined_names.push(entry.name.clone());
                                config.default = Some(entry.name.clone());
                                config.root_items.push(MenuItem::Entry(entry));
                            } else {
                                return Err(self.error("Expected entry after default".to_string()));
                            }
                        } else {
                            return Err(self.error("Expected entry after default".to_string()));
                        }
                    }
                    "entry" => {
                        let (entry, entry_warnings) = self.parse_entry()?;
                        warnings.extend(entry_warnings);
                        if defined_names.contains(&entry.name) {
                            return Err(self.error(format!("Duplicate entry name: {}", entry.name)));
                        }
                        defined_names.push(entry.name.clone());
                        config.root_items.push(MenuItem::Entry(entry));
                    }
                    "menu" => {
                        let (menu, menu_warnings) = self.parse_menu()?;
                        warnings.extend(menu_warnings);
                        if defined_names.contains(&menu.name) {
                            return Err(self.error(format!("Duplicate menu name: {}", menu.name)));
                        }
                        defined_names.push(menu.name.clone());
                        config.root_items.push(MenuItem::Menu(menu));
                    }
                    "set" => {
                        let (k, v) = self.parse_set()?;
                        config.root_items.push(MenuItem::Set(k, v));
                    }
                    _ => return Err(self.error(format!("Unexpected keyword at top level: {}", k))),
                },
                _ => return Err(self.error("Unexpected token at top level".to_string())),
            }
        }
        Ok((config, warnings))
    }

    fn expect_semicolon(&mut self) -> Result<(), ParseError> {
        match self.advance() {
            Some(Token {
                kind: TokenKind::Semicolon,
                ..
            }) => Ok(()),
            _ => Err(self.error("Expected semicolon".to_string())),
        }
    }

    fn parse_set(&mut self) -> Result<(String, String), ParseError> {
        self.expect_keyword("set")?;
        let key = match self.advance() {
            Some(Token {
                kind: TokenKind::Identifier(s),
                ..
            }) => s.clone(),
            _ => return Err(self.error("Expected identifier for set".to_string())),
        };
        match self.advance() {
            Some(Token {
                kind: TokenKind::Equals,
                ..
            }) => {}
            _ => return Err(self.error("Expected = in set".to_string())),
        }
        let value = self.expect_identifier_or_string()?;
        self.expect_semicolon()?;
        Ok((key, value))
    }

    fn parse_entry(&mut self) -> Result<(Entry, Vec<ParseWarning>), ParseError> {
        self.expect_keyword("entry")?;
        let name = self.expect_string()?;
        match self.advance() {
            Some(Token {
                kind: TokenKind::LBrace,
                ..
            }) => {}
            _ => return Err(self.error("Expected { after entry name".to_string())),
        }

        let mut statements = Vec::new();
        let mut warnings = Vec::new();
        let mut kernel_defined = false;
        let mut bootstrap_defined = false;

        while let Some(token) = self.peek() {
            match &token.kind {
                TokenKind::RBrace => {
                    self.advance();
                    return Ok((Entry { name, statements }, warnings));
                }
                TokenKind::Keyword(k) => match k.as_str() {
                    "set" => {
                        let (key, val) = self.parse_set()?;
                        statements.push(EntryStatement::Set(key, val));
                    }
                    "kernel" => {
                        if kernel_defined {
                            warnings.push(
                                self.warning(format!("Kernel redefined in entry '{}'", name)),
                            );
                        }
                        kernel_defined = true;
                        self.advance();
                        let path = self.expect_identifier_or_string()?;
                        let mut args = Vec::new();
                        // Parse args until semicolon
                        while let Some(t) = self.peek() {
                            if t.kind == TokenKind::Semicolon {
                                self.advance();
                                break;
                            }
                            match self.advance() {
                                Some(Token {
                                    kind: TokenKind::String(s),
                                    ..
                                }) => args.push(Arg::Expression(s.clone())),
                                Some(Token {
                                    kind: TokenKind::Identifier(s),
                                    ..
                                }) => args.push(Arg::Literal(s.clone())),
                                Some(Token {
                                    kind: TokenKind::Keyword(s),
                                    ..
                                }) => args.push(Arg::Literal(s.clone())),
                                Some(Token {
                                    kind: TokenKind::Number(n),
                                    ..
                                }) => args.push(Arg::Literal(n.to_string())),
                                _ => return Err(self.error("Expected argument".to_string())),
                            }
                        }
                        statements.push(EntryStatement::Kernel { path, args });
                    }
                    "bootstrap" => {
                        if bootstrap_defined {
                            warnings.push(
                                self.warning(format!("Bootstrap redefined in entry '{}'", name)),
                            );
                        }
                        bootstrap_defined = true;
                        self.advance();
                        let path = self.expect_identifier_or_string()?;
                        self.expect_semicolon()?;
                        statements.push(EntryStatement::Bootstrap { path });
                    }
                    "initramfs" => {
                        self.advance();
                        self.expect_semicolon()?;
                        statements.push(EntryStatement::Initramfs);
                    }
                    _ => return Err(self.error(format!("Unexpected keyword in entry: {}", k))),
                },
                _ => return Err(self.error("Unexpected token in entry".to_string())),
            }
        }
        Err(self.error("Unclosed entry block".to_string()))
    }

    fn parse_menu(&mut self) -> Result<(Menu, Vec<ParseWarning>), ParseError> {
        self.expect_keyword("menu")?;

        if let Some(Token {
            kind: TokenKind::Keyword(k),
            ..
        }) = self.peek()
        {
            if k == "entry" {
                self.advance();
            }
        }

        let name = self.expect_string()?;
        match self.advance() {
            Some(Token {
                kind: TokenKind::LBrace,
                ..
            }) => {}
            _ => return Err(self.error("Expected { after menu name".to_string())),
        }

        let mut items = Vec::new();
        let mut warnings = Vec::new();
        let mut defined_names = Vec::new();

        while let Some(token) = self.peek() {
            match &token.kind {
                TokenKind::RBrace => {
                    self.advance();
                    return Ok((Menu { name, items }, warnings));
                }
                TokenKind::Keyword(k) => match k.as_str() {
                    "entry" => {
                        let (entry, entry_warnings) = self.parse_entry()?;
                        warnings.extend(entry_warnings);
                        if defined_names.contains(&entry.name) {
                            return Err(self.error(format!(
                                "Duplicate entry name in menu '{}': {}",
                                name, entry.name
                            )));
                        }
                        defined_names.push(entry.name.clone());
                        items.push(MenuItem::Entry(entry));
                    }
                    "menu" => {
                        let (menu, menu_warnings) = self.parse_menu()?;
                        warnings.extend(menu_warnings);
                        if defined_names.contains(&menu.name) {
                            return Err(self.error(format!(
                                "Duplicate menu name in menu '{}': {}",
                                name, menu.name
                            )));
                        }
                        defined_names.push(menu.name.clone());
                        items.push(MenuItem::Menu(menu));
                    }
                    "set" => {
                        let (key, val) = self.parse_set()?;
                        items.push(MenuItem::Set(key, val));
                    }
                    _ => return Err(self.error(format!("Unexpected keyword in menu: {}", k))),
                },
                _ => return Err(self.error("Unexpected token in menu".to_string())),
            }
        }
        Err(self.error("Unclosed menu block".to_string()))
    }
}
