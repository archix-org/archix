//! Writes Lua table literals for generated config files.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Lua {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Lua>),
    /// Kept in insertion order, so generated files read top-down in a sensible order.
    Table(Vec<(String, Lua)>),
}

const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "then", "true", "until", "while",
];

const WIDTH: usize = 100;

impl Lua {
    pub fn str(s: impl Into<String>) -> Lua {
        Lua::Str(s.into())
    }

    pub fn strings<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> Lua {
        Lua::List(items.into_iter().map(|s| Lua::str(s.as_ref())).collect())
    }

    /// The literal at `indent` levels deep: on one line when it fits, else one entry per line.
    pub fn render(&self, indent: usize) -> String {
        let flat = self.flat();
        if flat.len() + indent * 2 <= WIDTH && !flat.contains('\n') {
            return flat;
        }
        let pad = "  ".repeat(indent + 1);
        let entries: Vec<String> = match self {
            Lua::List(items) => items.iter().map(|item| item.render(indent + 1)).collect(),
            Lua::Table(fields) => fields
                .iter()
                .map(|(key, value)| format!("{} = {}", key_of(key), value.render(indent + 1)))
                .collect(),
            _ => return flat,
        };
        let mut out = String::from("{\n");
        for entry in entries {
            let _ = writeln!(out, "{pad}{entry},");
        }
        out.push_str(&"  ".repeat(indent));
        out.push('}');
        out
    }

    fn flat(&self) -> String {
        match self {
            Lua::Str(s) => string(s),
            Lua::Int(n) => n.to_string(),
            Lua::Bool(b) => b.to_string(),
            Lua::List(items) if items.is_empty() => "{}".into(),
            Lua::Table(fields) if fields.is_empty() => "{}".into(),
            Lua::List(items) => {
                let items: Vec<String> = items.iter().map(Lua::flat).collect();
                format!("{{ {} }}", items.join(", "))
            }
            Lua::Table(fields) => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(key, value)| format!("{} = {}", key_of(key), value.flat()))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
        }
    }
}

fn key_of(key: &str) -> String {
    let mut chars = key.chars();
    let identifier = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&key);
    if identifier {
        key.to_string()
    } else {
        format!("[{}]", string(key))
    }
}

pub fn string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\{:03}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_tables_stay_on_one_line_and_long_ones_break() {
        let table = Lua::Table(vec![
            ("hostname".into(), Lua::str("box")),
            (
                "services".into(),
                Lua::Table(vec![("sshd.service".into(), Lua::Bool(true))]),
            ),
            ("end".into(), Lua::Int(1)),
        ]);
        assert_eq!(
            table.render(0),
            "{ hostname = \"box\", services = { [\"sshd.service\"] = true }, [\"end\"] = 1 }"
        );
        let long = Lua::Table(vec![("packages".into(), Lua::strings(["a-long-package-name"; 6]))]);
        assert_eq!(
            long.render(0),
            "{\n  packages = {\n    \"a-long-package-name\",\n    \"a-long-package-name\",\n    \"a-long-package-name\",\n    \"a-long-package-name\",\n    \"a-long-package-name\",\n    \"a-long-package-name\",\n  },\n}"
        );
        assert_eq!(string("a\"b\\c\nd\u{1}"), r#""a\"b\\c\nd\001""#);
    }
}
