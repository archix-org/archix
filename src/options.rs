use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::value::{Raw, Value};

pub const PRIO_FORCE: i32 = 50;
pub const PRIO_PLAIN: i32 = 100;
pub const PRIO_DEFAULT: i32 = 1000;
pub const PRIO_OPTION_DEFAULT: i32 = 1500;

pub type Children = BTreeMap<String, Node>;

#[derive(Debug, Clone)]
pub enum Node {
    Group(Rc<Children>),
    Opt(Rc<Decl>),
}

#[derive(Debug)]
pub struct Decl {
    pub ty: Type,
    pub default: Option<Raw>,
    pub description: Option<String>,
    pub source: Rc<str>,
}

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

pub fn declare(root: &mut Children, path: &[String], decl: Decl) -> Result<(), Diagnostic> {
    declare_in(root, "", path, decl)
}

fn declare_in(children: &mut Children, prefix: &str, rest: &[String], decl: Decl) -> Result<(), Diagnostic> {
    let (head, tail) = rest.split_first().expect("option path is never empty");
    let here = join(prefix, head);
    if tail.is_empty() {
        return match children.get(head) {
            Some(Node::Opt(existing)) => Err(Diagnostic::new(
                &here,
                format!("option is declared twice, in {} and {}", existing.source, decl.source),
            )),
            Some(Node::Group(_)) => Err(Diagnostic::new(
                &here,
                format!(
                    "declared as an option in {} but other options are declared under it",
                    decl.source
                ),
            )),
            None => {
                children.insert(head.clone(), Node::Opt(Rc::new(decl)));
                Ok(())
            }
        };
    }
    let entry = children
        .entry(head.clone())
        .or_insert_with(|| Node::Group(Rc::default()));
    match entry {
        Node::Opt(existing) => Err(Diagnostic::new(
            &here,
            format!(
                "is an option declared in {}, so {} can't declare options under it",
                existing.source, decl.source
            ),
        )),
        Node::Group(group) => declare_in(Rc::make_mut(group), &here, tail, decl),
    }
}

/// Every declared option with its display path, including options inside submodules.
pub fn flatten(children: &Children) -> Vec<(String, Rc<Decl>)> {
    let mut out = Vec::new();
    flatten_into(children, "", &mut out);
    out
}

fn flatten_into(children: &Children, prefix: &str, out: &mut Vec<(String, Rc<Decl>)>) {
    for (name, node) in children {
        let path = join(prefix, name);
        match node {
            Node::Group(group) => flatten_into(group, &path, out),
            Node::Opt(decl) => {
                out.push((path.clone(), decl.clone()));
                flatten_type(&decl.ty, &path, out);
            }
        }
    }
}

fn flatten_type(ty: &Type, path: &str, out: &mut Vec<(String, Rc<Decl>)>) {
    match ty {
        Type::Submodule(children) => flatten_into(children, path, out),
        Type::List(elem) => flatten_type(elem, &format!("{path}.*"), out),
        Type::Attrs(elem) => flatten_type(elem, &format!("{path}.<name>"), out),
        Type::Nullable(elem) => flatten_type(elem, path, out),
        _ => {}
    }
}

/// A "; did you mean `x`?" suffix naming the closest sibling of an unknown key, or "".
pub fn did_you_mean(path: &str, key: &str, children: &Children) -> String {
    children
        .keys()
        .map(|name| (strsim::damerau_levenshtein(name, key), name))
        .filter(|(distance, _)| *distance <= 2)
        .min()
        .map(|(_, name)| format!("; did you mean `{}`?", join(path, name)))
        .unwrap_or_default()
}

pub fn merge(root: &Children, defs: Vec<Def>) -> (Value, Vec<Diagnostic>) {
    let mut merger = Merger::default();
    let value = merger
        .group("", root, defs)
        .unwrap_or_else(|| Value::Map(BTreeMap::new()));
    (value, merger.errors)
}

/// Checks one definition against a type, as done for option defaults.
pub fn check(path: &str, ty: &Type, def: Def) -> Result<Value, Vec<Diagnostic>> {
    let mut merger = Merger::default();
    let value = merger.typed(path, ty, vec![def]);
    match value {
        Some(value) if merger.errors.is_empty() => Ok(value),
        _ => Err(merger.errors),
    }
}

#[derive(Default)]
struct Merger {
    errors: Vec<Diagnostic>,
}

impl Merger {
    fn err(&mut self, path: &str, message: String) {
        self.errors.push(Diagnostic::new(path, message));
    }

    fn unwrap_prio(&mut self, path: &str, def: Def) -> Option<Def> {
        match def.raw {
            Raw::Prio(_, inner) if matches!(*inner, Raw::Prio(..)) => {
                self.err(path, format!("nested archix.force/default wrappers in {}", def.source));
                None
            }
            Raw::Prio(prio, inner) => Some(Def {
                raw: *inner,
                prio: Some(prio),
                source: def.source,
            }),
            _ => Some(def),
        }
    }

    fn group(&mut self, path: &str, children: &Children, defs: Vec<Def>) -> Option<Value> {
        let mut buckets: BTreeMap<String, Vec<Def>> = BTreeMap::new();
        for def in defs {
            let Some(def) = self.unwrap_prio(path, def) else {
                continue;
            };
            match def.raw {
                Raw::Map(map) => {
                    for (key, raw) in map {
                        if children.contains_key(&key) {
                            let sub = Def {
                                raw,
                                prio: def.prio,
                                source: def.source.clone(),
                            };
                            buckets.entry(key).or_default().push(sub);
                        } else {
                            self.unknown(path, &key, children, &def.source);
                        }
                    }
                }
                Raw::Empty => {}
                other => self.err(
                    path,
                    format!(
                        "expected a table of options, got {} in {}",
                        describe(&other),
                        def.source
                    ),
                ),
            }
        }
        let mut out = BTreeMap::new();
        for (name, node) in children {
            let defs = buckets.remove(name).unwrap_or_default();
            let sub = join(path, name);
            let value = match node {
                Node::Group(group) => self.group(&sub, group, defs),
                Node::Opt(decl) => {
                    let default = decl.default.as_ref().map(|raw| (raw, &decl.source));
                    self.option(&sub, &decl.ty, default, defs)
                }
            };
            if let Some(value) = value {
                out.insert(name.clone(), value);
            }
        }
        (!out.is_empty()).then_some(Value::Map(out))
    }

    fn unknown(&mut self, path: &str, key: &str, children: &Children, source: &str) {
        let message = format!(
            "option does not exist (set in {source}){}",
            did_you_mean(path, key, children)
        );
        self.err(&join(path, key), message);
    }

    fn option(&mut self, path: &str, ty: &Type, default: Option<(&Raw, &Rc<str>)>, defs: Vec<Def>) -> Option<Value> {
        let mut all = Vec::with_capacity(defs.len() + 1);
        if let Some((raw, source)) = default {
            let source: Rc<str> = format!("{source} (default)").into();
            all.push(Def {
                raw: raw.clone(),
                prio: Some(PRIO_OPTION_DEFAULT),
                source,
            });
        }
        for def in defs {
            if let Some(mut def) = self.unwrap_prio(path, def) {
                def.prio.get_or_insert(PRIO_PLAIN);
                all.push(def);
            }
        }
        let best = all.iter().filter_map(|def| def.prio).min()?;
        all.retain(|def| def.prio == Some(best));
        self.typed(path, ty, all)
    }

    fn typed(&mut self, path: &str, ty: &Type, defs: Vec<Def>) -> Option<Value> {
        match ty {
            Type::Submodule(children) => Some(
                self.group(path, children, defs)
                    .unwrap_or_else(|| Value::Map(BTreeMap::new())),
            ),
            Type::List(elem) => {
                let mut out = Vec::new();
                for def in defs {
                    match def.raw {
                        Raw::List(items) => {
                            for (i, raw) in items.into_iter().enumerate() {
                                let item = Def::new(raw, def.source.clone());
                                let item_path = format!("{path}[{}]", i + 1);
                                out.extend(self.typed(&item_path, elem, vec![item]));
                            }
                        }
                        Raw::Empty => {}
                        other => self.mismatch(path, ty, &other, &def.source),
                    }
                }
                Some(Value::List(out))
            }
            Type::Attrs(elem) => {
                let mut buckets: BTreeMap<String, Vec<Def>> = BTreeMap::new();
                for def in defs {
                    match def.raw {
                        Raw::Map(map) => {
                            for (key, raw) in map {
                                buckets.entry(key).or_default().push(Def::new(raw, def.source.clone()));
                            }
                        }
                        Raw::Empty => {}
                        other => self.mismatch(path, ty, &other, &def.source),
                    }
                }
                let mut out = BTreeMap::new();
                for (key, defs) in buckets {
                    if let Some(value) = self.option(&join(path, &key), elem, None, defs) {
                        out.insert(key, value);
                    }
                }
                Some(Value::Map(out))
            }
            Type::Nullable(elem) => {
                let nulls = defs.iter().filter(|def| def.raw == Raw::Null).count();
                if nulls == defs.len() {
                    Some(Value::Null)
                } else if nulls > 0 {
                    let items = defs
                        .iter()
                        .map(|def| (describe(&def.raw), def.source.clone()))
                        .collect();
                    self.conflict(path, items);
                    None
                } else {
                    self.typed(path, elem, defs)
                }
            }
            Type::Lines => {
                let mut parts = Vec::new();
                for def in defs {
                    match def.raw {
                        Raw::Str(s) => parts.push(s),
                        other => self.mismatch(path, ty, &other, &def.source),
                    }
                }
                Some(Value::Str(parts.join("\n")))
            }
            Type::Any if defs.iter().all(|def| matches!(def.raw, Raw::Map(_) | Raw::Empty)) => {
                self.typed(path, &Type::Attrs(Box::new(Type::Any)), defs)
            }
            Type::Any if defs.iter().all(|def| matches!(def.raw, Raw::List(_) | Raw::Empty)) => {
                self.typed(path, &Type::List(Box::new(Type::Any)), defs)
            }
            _ => self.scalar(path, ty, defs),
        }
    }

    fn scalar(&mut self, path: &str, ty: &Type, defs: Vec<Def>) -> Option<Value> {
        let mut values: Vec<(Value, Rc<str>)> = Vec::new();
        for def in defs {
            if let Some(value) = self.leaf(path, ty, def.raw, &def.source) {
                values.push((value, def.source));
            }
        }
        let (first, _) = values.first()?;
        if values.iter().all(|(value, _)| value == first) {
            return Some(first.clone());
        }
        let items = values
            .into_iter()
            .map(|(value, source)| (value.to_string(), source))
            .collect();
        self.conflict(path, items);
        None
    }

    fn leaf(&mut self, path: &str, ty: &Type, raw: Raw, source: &str) -> Option<Value> {
        match (ty, raw) {
            (Type::Bool | Type::Any, Raw::Bool(b)) => Some(Value::Bool(b)),
            (Type::Int | Type::Any, Raw::Int(i)) => Some(Value::Int(i)),
            (Type::Float, Raw::Int(i)) => Some(Value::Float(i as f64)),
            (Type::Float | Type::Any, Raw::Float(f)) => Some(Value::Float(f)),
            (Type::Str | Type::Any, Raw::Str(s)) => Some(Value::Str(s)),
            (Type::Any, Raw::Null) => Some(Value::Null),
            (Type::Path, Raw::Str(s)) if s.starts_with('/') || s == "~" || s.starts_with("~/") => Some(Value::Str(s)),
            (Type::Path, Raw::Str(s)) => {
                self.err(
                    path,
                    format!("expected an absolute path or one starting with ~/, got {s:?} in {source}"),
                );
                None
            }
            (Type::Enum(values), Raw::Str(s)) if values.contains(&s) => Some(Value::Str(s)),
            (_, raw) => {
                self.mismatch(path, ty, &raw, source);
                None
            }
        }
    }

    fn mismatch(&mut self, path: &str, ty: &Type, raw: &Raw, source: &str) {
        let hint = match raw {
            Raw::Null => "; wrap the type in types.nullable() to allow null",
            Raw::Prio(..) => "; archix.force/default can only wrap a whole option value or a table of options",
            _ => "",
        };
        self.err(path, format!("expected {ty}, got {} in {source}{hint}", describe(raw)));
    }

    fn conflict(&mut self, path: &str, items: Vec<(String, Rc<str>)>) {
        let mut message = String::from("conflicting definitions:");
        for (value, source) in items {
            message.push_str(&format!("\n    {value} in {source}"));
        }
        message.push_str("\n  use archix.force() on the one that should win");
        self.err(path, message);
    }
}

fn describe(raw: &Raw) -> String {
    match raw {
        Raw::Bool(b) => format!("boolean {b}"),
        Raw::Int(i) => format!("integer {i}"),
        Raw::Float(f) => format!("float {f}"),
        Raw::Str(s) => format!("string {s:?}"),
        other => other.kind().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(ty: Type, default: Option<Raw>) -> Node {
        Node::Opt(Rc::new(Decl {
            ty,
            default,
            description: None,
            source: "decl.lua".into(),
        }))
    }

    fn map(entries: Vec<(&str, Raw)>) -> Raw {
        Raw::Map(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn def(source: &str, raw: Raw) -> Def {
        Def::new(raw, source.into())
    }

    fn tree(entries: Vec<(&str, Node)>) -> Children {
        entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
        match value {
            Value::Map(map) => &map[key],
            _ => panic!("not a map: {value:?}"),
        }
    }

    #[test]
    fn default_applies_when_unset() {
        let root = tree(vec![("size", opt(Type::Int, Some(Raw::Int(12))))]);
        let (value, errors) = merge(&root, vec![]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(get(&value, "size"), &Value::Int(12));
    }

    #[test]
    fn equal_scalars_merge_and_different_ones_conflict() {
        let root = tree(vec![("size", opt(Type::Int, None))]);
        let (_, errors) = merge(
            &root,
            vec![
                def("a.lua", map(vec![("size", Raw::Int(1))])),
                def("b.lua", map(vec![("size", Raw::Int(1))])),
            ],
        );
        assert!(errors.is_empty());
        let (_, errors) = merge(
            &root,
            vec![
                def("a.lua", map(vec![("size", Raw::Int(1))])),
                def("b.lua", map(vec![("size", Raw::Int(2))])),
            ],
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("a.lua") && errors[0].message.contains("b.lua"));
    }

    #[test]
    fn force_beats_plain_beats_default() {
        let root = tree(vec![("size", opt(Type::Int, Some(Raw::Int(0))))]);
        let (value, errors) = merge(
            &root,
            vec![
                def(
                    "a.lua",
                    map(vec![("size", Raw::Prio(PRIO_DEFAULT, Box::new(Raw::Int(1))))]),
                ),
                def("b.lua", map(vec![("size", Raw::Int(2))])),
                def(
                    "c.lua",
                    map(vec![("size", Raw::Prio(PRIO_FORCE, Box::new(Raw::Int(3))))]),
                ),
            ],
        );
        assert!(errors.is_empty());
        assert_eq!(get(&value, "size"), &Value::Int(3));
    }

    #[test]
    fn priority_on_a_group_pushes_down() {
        let root = tree(vec![(
            "font",
            Node::Group(Rc::new(tree(vec![
                ("size", opt(Type::Int, None)),
                ("name", opt(Type::Str, None)),
            ]))),
        )]);
        let forced = Raw::Prio(PRIO_FORCE, Box::new(map(vec![("size", Raw::Int(11))])));
        let (value, errors) = merge(
            &root,
            vec![
                def(
                    "shared.lua",
                    map(vec![(
                        "font",
                        map(vec![("size", Raw::Int(12)), ("name", Raw::Str("x".into()))]),
                    )]),
                ),
                def("laptop.lua", map(vec![("font", forced)])),
            ],
        );
        assert!(errors.is_empty(), "{errors:?}");
        let font = get(&value, "font");
        assert_eq!(get(font, "size"), &Value::Int(11));
        assert_eq!(get(font, "name"), &Value::Str("x".into()));
    }

    #[test]
    fn lists_concatenate_in_definition_order() {
        let root = tree(vec![("packages", opt(Type::List(Box::new(Type::Str)), None))]);
        let (value, _) = merge(
            &root,
            vec![
                def(
                    "a.lua",
                    map(vec![("packages", Raw::List(vec![Raw::Str("git".into())]))]),
                ),
                def("b.lua", map(vec![("packages", Raw::Empty)])),
                def(
                    "c.lua",
                    map(vec![("packages", Raw::List(vec![Raw::Str("kitty".into())]))]),
                ),
            ],
        );
        assert_eq!(
            get(&value, "packages"),
            &Value::List(vec![Value::Str("git".into()), Value::Str("kitty".into())])
        );
    }

    #[test]
    fn unknown_option_suggests_close_name() {
        let root = tree(vec![("kitty", opt(Type::Bool, None))]);
        let (_, errors) = merge(&root, vec![def("a.lua", map(vec![("kity", Raw::Bool(true))]))]);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path.as_deref(), Some("kity"));
        assert!(errors[0].message.contains("did you mean `kitty`"));
    }

    #[test]
    fn type_errors_name_the_source() {
        let root = tree(vec![("size", opt(Type::Int, None))]);
        let (_, errors) = merge(&root, vec![def("a.lua", map(vec![("size", Raw::Str("big".into()))]))]);
        assert_eq!(errors[0].message, "expected int, got string \"big\" in a.lua");
    }

    #[test]
    fn attrs_of_submodule_merges_per_key_with_field_defaults() {
        let user = Type::Submodule(Rc::new(tree(vec![
            ("shell", opt(Type::Str, Some(Raw::Str("bash".into())))),
            ("groups", opt(Type::List(Box::new(Type::Str)), Some(Raw::Empty))),
        ])));
        let root = tree(vec![("users", opt(Type::Attrs(Box::new(user)), None))]);
        let (value, errors) = merge(
            &root,
            vec![
                def(
                    "a.lua",
                    map(vec![(
                        "users",
                        map(vec![(
                            "alice",
                            map(vec![("groups", Raw::List(vec![Raw::Str("wheel".into())]))]),
                        )]),
                    )]),
                ),
                def(
                    "b.lua",
                    map(vec![(
                        "users",
                        map(vec![
                            ("alice", map(vec![("shell", Raw::Str("zsh".into()))])),
                            ("bob", Raw::Empty),
                        ]),
                    )]),
                ),
            ],
        );
        assert!(errors.is_empty(), "{errors:?}");
        let users = get(&value, "users");
        assert_eq!(get(get(users, "alice"), "shell"), &Value::Str("zsh".into()));
        assert_eq!(
            get(get(users, "alice"), "groups"),
            &Value::List(vec![Value::Str("wheel".into())])
        );
        assert_eq!(get(get(users, "bob"), "shell"), &Value::Str("bash".into()));
    }

    #[test]
    fn nullable_accepts_null_but_plain_types_do_not() {
        let root = tree(vec![
            (
                "a",
                opt(Type::Nullable(Box::new(Type::Str)), Some(Raw::Str("x".into()))),
            ),
            ("b", opt(Type::Str, None)),
        ]);
        let (value, errors) = merge(&root, vec![def("m.lua", map(vec![("a", Raw::Null), ("b", Raw::Null)]))]);
        assert_eq!(get(&value, "a"), &Value::Null);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("types.nullable"));
    }

    #[test]
    fn lines_join_with_newlines() {
        let root = tree(vec![("extra", opt(Type::Lines, None))]);
        let (value, _) = merge(
            &root,
            vec![
                def("a.lua", map(vec![("extra", Raw::Str("one".into()))])),
                def("b.lua", map(vec![("extra", Raw::Str("two".into()))])),
            ],
        );
        assert_eq!(get(&value, "extra"), &Value::Str("one\ntwo".into()));
    }

    #[test]
    fn declaring_twice_is_an_error() {
        let mut root = Children::new();
        let decl = || Decl {
            ty: Type::Bool,
            default: None,
            description: None,
            source: "x.lua".into(),
        };
        let path = vec!["programs".to_string(), "kitty".to_string()];
        declare(&mut root, &path, decl()).unwrap();
        assert!(declare(&mut root, &path, decl()).is_err());
        assert!(declare(&mut root, &path[..1], decl()).is_err());
    }
}
