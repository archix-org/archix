use std::collections::BTreeMap;
use std::rc::Rc;

use super::{Diagnostic, Type, join};
use crate::value::Raw;

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

#[cfg(test)]
mod tests {
    use super::*;

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
