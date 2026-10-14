use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| fmt::Error)?)
    }
}

/// An unchecked config value as written in Lua, before the option types are applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Raw>),
    Map(BTreeMap<String, Raw>),
    /// `{}` is both an empty list and an empty map; the option type decides.
    Empty,
    Prio(i32, Box<Raw>),
}

impl Raw {
    pub fn kind(&self) -> &'static str {
        match self {
            Raw::Null => "null",
            Raw::Bool(_) => "boolean",
            Raw::Int(_) => "integer",
            Raw::Float(_) => "float",
            Raw::Str(_) => "string",
            Raw::List(_) => "list",
            Raw::Map(_) => "table",
            Raw::Empty => "empty table",
            Raw::Prio(..) => "priority wrapper",
        }
    }
}
