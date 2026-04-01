use alloc::{string::String, vec::Vec};

#[derive(Debug, Clone)]
pub struct Config {
    pub timeout: Option<u64>,
    pub default: Option<String>,
    pub root_items: Vec<MenuItem>,
}

#[derive(Debug, Clone)]
pub enum MenuItem {
    Entry(Entry),
    Menu(Menu),
    Set(String, String),
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub name: String,
    pub items: Vec<MenuItem>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub statements: Vec<EntryStatement>,
}

#[derive(Debug, Clone)]
pub enum EntryStatement {
    Set(String, String),
    Kernel { path: String, args: Vec<Arg> },
    Bootstrap { path: String },
    Initramfs,
}

#[derive(Debug, Clone)]
pub enum Arg {
    Literal(String),
    Expression(String),
}
