use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};

pub struct Evaluator {
    scopes: Vec<BTreeMap<String, String>>,
}

impl Evaluator {
    pub fn new() -> Self {
        Self {
            scopes: vec![BTreeMap::new()],
        }
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    pub fn set(&mut self, key: String, value: String) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(key, value);
        }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        for scope in self.scopes.iter().rev() {
            if let Some(val) = scope.get(key) {
                return Some(val);
            }
        }
        None
    }

    pub fn eval_string(&self, input: &str) -> String {
        let mut result = String::new();
        let mut chars = input.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '{' {
                if let Some(&next) = chars.peek() {
                    if next == '{' {
                        result.push('{');
                        chars.next();
                    } else {
                        // Variable substitution
                        let mut var_name = String::new();
                        while let Some(&vc) = chars.peek() {
                            if vc == '}' {
                                chars.next();
                                break;
                            }
                            var_name.push(vc);
                            chars.next();
                        }
                        if let Some(val) = self.get(&var_name) {
                            result.push_str(val);
                        }
                    }
                } else {
                    result.push('{');
                }
            } else if c == '}' {
                if let Some(&next) = chars.peek() {
                    if next == '}' {
                        result.push('}');
                        chars.next();
                    } else {
                        result.push('}');
                    }
                } else {
                    result.push('}');
                }
            } else if c == '\\' {
                if let Some(&next) = chars.peek() {
                    result.push(next);
                    chars.next();
                }
            } else {
                result.push(c);
            }
        }
        result
    }
}
