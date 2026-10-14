use std::fmt;
use std::rc::Rc;

use super::Children;

#[derive(Debug, Clone)]
pub enum Type {
    Bool,
    Int,
    Float,
    Str,
    Path,
    Lines,
    Any,
    Enum(Vec<String>),
    List(Box<Type>),
    Attrs(Box<Type>),
    Nullable(Box<Type>),
    Submodule(Rc<Children>),
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Bool => f.write_str("bool"),
            Type::Int => f.write_str("int"),
            Type::Float => f.write_str("float"),
            Type::Str => f.write_str("str"),
            Type::Path => f.write_str("path"),
            Type::Lines => f.write_str("lines"),
            Type::Any => f.write_str("any"),
            Type::Enum(values) => {
                let quoted: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
                write!(f, "one of {}", quoted.join(", "))
            }
            Type::List(elem) => write!(f, "list of {elem}"),
            Type::Attrs(elem) => write!(f, "attrs of {elem}"),
            Type::Nullable(elem) => write!(f, "null or {elem}"),
            Type::Submodule(_) => f.write_str("submodule"),
        }
    }
}
