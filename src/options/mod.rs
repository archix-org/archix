mod merge;
mod tree;
mod types;

use std::fmt;
use std::rc::Rc;

use crate::value::Raw;

pub use merge::{check, merge};
pub use tree::{Children, Decl, Node, declare, did_you_mean, flatten};
pub use types::Type;

pub const PRIO_FORCE: i32 = 50;
pub const PRIO_PLAIN: i32 = 100;
pub const PRIO_DEFAULT: i32 = 1000;
pub const PRIO_OPTION_DEFAULT: i32 = 1500;

#[derive(Debug, Clone)]
pub struct Def {
    pub raw: Raw,
    pub prio: Option<i32>,
    pub source: Rc<str>,
}

impl Def {
    pub fn new(raw: Raw, source: Rc<str>) -> Self {
        Def {
            raw,
            prio: None,
            source,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub path: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn new(path: &str, message: impl Into<String>) -> Self {
        let path = (!path.is_empty()).then(|| path.to_string());
        Diagnostic {
            path,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "`{path}`: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

pub fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}
