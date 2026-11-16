//! Value → Nix text for home.nix, by SCOPE 7's mapping rules.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{Result, bail};

use super::{HM_TAG, Refusal, is_tainted};
use crate::value::Value;

const KEYWORDS: [&str; 10] = [
    "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or",
];

/// What rendering found besides the text: each hm.path as (option path, source relative to the config tree).
#[derive(Debug, Default, PartialEq)]
pub struct Found {
    pub paths: Vec<(String, String)>,
}

/// `value` as a Nix expression; `path` names the option in errors.
pub fn expr(value: &Value, path: &str, found: &mut Found) -> Result<String> {
    let mut out = String::new();
    write_value(&mut out, value, path, 0, found)?;
    Ok(out)
}

/// A Nix double-quoted string literal holding `s`.
pub fn string(s: &str, path: &str) -> Result<String> {
    if s.contains('\0') {
        bail!("{path}: a string holds a NUL character, which Nix strings can't");
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '\'' | '-'))
        && !KEYWORDS.contains(&name)
}

/// An attribute name, bare when it's an identifier and quoted otherwise.
pub fn attr_name(name: &str, path: &str) -> Result<String> {
    match is_identifier(name) {
        true => Ok(name.to_string()),
        false => string(name, path),
    }
}

pub fn int(i: i64) -> String {
    match i {
        i64::MIN => "(-9223372036854775807 - 1)".into(),
        i if i < 0 => format!("({i})"),
        i => i.to_string(),
    }
}

pub fn float(f: f64, path: &str) -> Result<String> {
    if !f.is_finite() {
        bail!("{path}: {f} has no Nix float; use a finite number");
    }
    let mut text = format!("{:?}", f.abs());
    if let Some(e) = text.find('e')
        && !text[..e].contains('.')
    {
        text.insert_str(e, ".0");
    }
    Ok(match f.is_sign_negative() {
        true => format!("(-{text})"),
        false => text,
    })
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn join(path: &str, key: &str) -> String {
    match is_identifier(key) {
        true => format!("{path}.{key}"),
        false => format!("{path}.{key:?}"),
    }
}

fn write_value(out: &mut String, value: &Value, path: &str, depth: usize, found: &mut Found) -> Result<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&int(*i)),
        Value::Float(f) => out.push_str(&float(*f, path)?),
        Value::Str(s) => {
            if is_tainted(s) {
                return Err(Refusal::secret(path).into());
            }
            out.push_str(&string(s, path)?);
        }
        Value::List(items) => {
            if items.is_empty() {
                out.push_str("[ ]");
                return Ok(());
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                indent(out, depth + 1);
                let sub = format!("{path}[{}]", i + 1);
                let mut text = String::new();
                write_value(&mut text, item, &sub, depth + 1, found)?;
                out.push_str(&text);
                out.push('\n');
            }
            indent(out, depth);
            out.push(']');
        }
        Value::Map(map) if map.contains_key(HM_TAG) => write_tagged(out, map, path, found)?,
        Value::Map(map) if map.is_empty() => {
            bail!("{path}: an empty table is neither a list nor an attrset in Nix; write hm.list{{}} or hm.attrs{{}}")
        }
        Value::Map(map) => {
            out.push_str("{\n");
            for (key, item) in map {
                let sub = join(path, key);
                if is_tainted(key) {
                    return Err(Refusal::secret(&sub).into());
                }
                indent(out, depth + 1);
                out.push_str(&attr_name(key, &sub)?);
                out.push_str(" = ");
                write_value(out, item, &sub, depth + 1, found)?;
                out.push_str(";\n");
            }
            indent(out, depth);
            out.push('}');
        }
    }
    Ok(())
}

fn tag_field<'a>(map: &'a BTreeMap<String, Value>, field: &str, path: &str) -> Result<&'a str> {
    match map.get(field) {
        Some(Value::Str(s)) if is_tainted(s) => Err(Refusal::secret(path).into()),
        Some(Value::Str(s)) => Ok(s),
        _ => bail!("{path}: a malformed hm value (no {field}); build it with the hm.* helpers"),
    }
}

fn write_tagged(out: &mut String, map: &BTreeMap<String, Value>, path: &str, found: &mut Found) -> Result<()> {
    let kind = tag_field(map, HM_TAG, path)?;
    let fields = match kind {
        "list" | "attrs" => 1,
        _ => 2,
    };
    if map.len() != fields {
        bail!("{path}: a {kind} hm value merged with other definitions; give it in one place");
    }
    match kind {
        "list" => out.push_str("[ ]"),
        "attrs" => out.push_str("{ }"),
        "pkg" => {
            let name = tag_field(map, "name", path)?;
            out.push_str("pkgs");
            for part in name.split('.') {
                if part.is_empty() {
                    bail!("{path}: hm.pkg {name:?} has an empty attribute name");
                }
                out.push('.');
                out.push_str(&attr_name(part, path)?);
            }
        }
        "raw" => {
            let expr = tag_field(map, "expr", path)?;
            if expr.contains('\0') {
                bail!("{path}: hm.raw holds a NUL character");
            }
            let _ = write!(out, "(\n{expr}\n)");
        }
        "path" => {
            let rel = normalize(tag_field(map, "path", path)?, path)?;
            out.push_str(&path_expr(&rel, path)?);
            found.paths.push((path.to_string(), rel));
        }
        other => bail!("{path}: unknown hm value kind {other:?}"),
    }
    Ok(())
}

/// `rel` with `.` parts and repeated slashes dropped; absolute paths and `..` are refused.
pub fn normalize(rel: &str, path: &str) -> Result<String> {
    if rel.starts_with('/') || rel.starts_with('~') {
        bail!("{path}: hm.path {rel:?} must be relative to the config directory");
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("{path}: hm.path {rel:?} leaves the config directory"),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        bail!("{path}: hm.path {rel:?} names the config directory itself");
    }
    Ok(parts.join("/"))
}

fn path_expr(rel: &str, path: &str) -> Result<String> {
    let literal = rel
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '/'));
    match literal {
        true => Ok(format!("./files/{rel}")),
        false => Ok(format!("(./files + {})", string(&format!("/{rel}"), path)?)),
    }
}

/// home.nix for one user's home_manager value, which must be an attrset.
pub fn home_nix(value: &Value, path: &str) -> Result<(String, Found)> {
    if !matches!(value, Value::Map(map) if !map.contains_key(HM_TAG)) {
        bail!("{path}: expected a table of home-manager options");
    }
    let mut found = Found::default();
    let body = match value {
        Value::Map(map) if map.is_empty() => "{ }".to_string(),
        value => expr(value, path, &mut found)?,
    };
    let text =
        format!("# Generated by archix from {path}; a switch overwrites it.\n{{ config, lib, pkgs, ... }}:\n{body}\n");
    Ok((text, found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::hm::SECRET_MESSAGE;

    fn s(text: &str) -> String {
        string(text, "x").unwrap()
    }

    #[test]
    fn strings_escape_backslash_quote_interpolation_and_controls() {
        assert_eq!(s(r#"a\b"c"#), r#""a\\b\"c""#);
        assert_eq!(s("${x} $y $${z}"), r#""\${x} $y $\${z}""#);
        assert_eq!(s("a\nb\tc\rd"), r#""a\nb\tc\rd""#);
        assert_eq!(s("''x'' é ✓"), "\"''x'' é ✓\"");
        assert!(string("a\0b", "opt.x").unwrap_err().to_string().contains("opt.x"));
    }

    #[test]
    fn attribute_names_are_bare_only_when_they_are_identifiers() {
        assert_eq!(attr_name("enable", "p").unwrap(), "enable");
        assert_eq!(attr_name("font-size'", "p").unwrap(), "font-size'");
        for name in ["", "1x", "a.b", "with", "or", "a b", "${x}", "é"] {
            assert!(attr_name(name, "p").unwrap().starts_with('"'), "{name}");
        }
    }

    #[test]
    fn numbers_keep_their_kind_and_refuse_non_finite_floats() {
        assert_eq!(int(5), "5");
        assert_eq!(int(-5), "(-5)");
        assert_eq!(int(i64::MIN), "(-9223372036854775807 - 1)");
        assert_eq!(float(1.0, "p").unwrap(), "1.0");
        assert_eq!(float(-0.5, "p").unwrap(), "(-0.5)");
        assert_eq!(float(1e-7, "p").unwrap(), "1.0e-7");
        assert_eq!(float(1.5e300, "p").unwrap(), "1.5e300");
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                float(bad, "home.u.home_manager.x")
                    .unwrap_err()
                    .to_string()
                    .contains("home.u.home_manager.x")
            );
        }
    }

    fn tagged(kind: &str, field: Option<(&str, &str)>) -> Value {
        let mut map = BTreeMap::from([(HM_TAG.to_string(), Value::Str(kind.into()))]);
        if let Some((k, v)) = field {
            map.insert(k.into(), Value::Str(v.into()));
        }
        Value::Map(map)
    }

    #[test]
    fn tagged_values_render_as_nix() {
        let mut found = Found::default();
        let mut render = |v: &Value| expr(v, "p", &mut found).unwrap();
        assert_eq!(render(&tagged("list", None)), "[ ]");
        assert_eq!(render(&tagged("attrs", None)), "{ }");
        assert_eq!(render(&tagged("pkg", Some(("name", "ripgrep")))), "pkgs.ripgrep");
        assert_eq!(
            render(&tagged("pkg", Some(("name", "python3Packages.a+b")))),
            "pkgs.python3Packages.\"a+b\""
        );
        assert_eq!(
            render(&tagged("raw", Some(("expr", "lib.mkForce true")))),
            "(\nlib.mkForce true\n)"
        );
        assert_eq!(
            render(&tagged("path", Some(("path", "hypr//./a.conf")))),
            "./files/hypr/a.conf"
        );
        assert_eq!(
            render(&tagged("path", Some(("path", "a b/c")))),
            "(./files + \"/a b/c\")"
        );
        let rels: Vec<&str> = found.paths.iter().map(|(_, rel)| rel.as_str()).collect();
        assert_eq!(rels, ["hypr/a.conf", "a b/c"]);
    }

    #[test]
    fn empty_tables_and_escaping_paths_are_errors() {
        let mut found = Found::default();
        let empty = Value::Map(BTreeMap::new());
        let err = expr(&empty, "home.u.home_manager.x", &mut found)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("hm.list{}") && err.contains("home.u.home_manager.x"),
            "{err}"
        );
        for bad in ["../x", "/etc/passwd", "a/../../b", "."] {
            assert!(
                expr(&tagged("path", Some(("path", bad))), "p", &mut found).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_secret_placeholder_anywhere_is_refused_naming_the_option() {
        let secret = crate::secrets::placeholder("token");
        let cases = [
            Value::Map(BTreeMap::from([(
                "programs".into(),
                Value::Map(BTreeMap::from([("x".into(), Value::Str(format!("a{secret}b")))])),
            )])),
            Value::Map(BTreeMap::from([(secret.clone(), Value::Bool(true))])),
            Value::Map(BTreeMap::from([("r".into(), tagged("raw", Some(("expr", &secret))))])),
        ];
        for value in cases {
            let err = expr(&value, "home.u.home_manager", &mut Found::default()).unwrap_err();
            let refusal = err.downcast_ref::<Refusal>().expect("a refusal");
            assert!(refusal.0.starts_with("home.u.home_manager"), "{}", refusal.0);
            assert!(refusal.0.contains(SECRET_MESSAGE), "{}", refusal.0);
        }
    }
}
