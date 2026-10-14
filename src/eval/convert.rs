use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use mlua::{Lua, Table, Value as LuaValue};

use super::sandbox::{Markers, UnsetReads};
use crate::options::{self, Children, Decl, Def, Diagnostic, Node, Type};
use crate::value::{Raw, Value};

const MAX_DEPTH: usize = 64;

fn item_path(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

pub fn to_raw(markers: &Markers, value: &LuaValue, path: &str) -> Result<Raw, Diagnostic> {
    to_raw_at(markers, value, path, 0)
}

fn to_raw_at(markers: &Markers, value: &LuaValue, path: &str, depth: usize) -> Result<Raw, Diagnostic> {
    let fail = |message: String| Diagnostic::new(path, message);
    match value {
        LuaValue::Boolean(b) => Ok(Raw::Bool(*b)),
        LuaValue::Integer(i) => Ok(Raw::Int(*i)),
        LuaValue::Number(f) => Ok(Raw::Float(*f)),
        LuaValue::String(s) => match s.to_str() {
            Ok(s) => Ok(Raw::Str(s.to_string())),
            Err(_) => Err(fail("string is not valid UTF-8".into())),
        },
        LuaValue::Table(t) => {
            if depth > MAX_DEPTH {
                return Err(fail(format!(
                    "tables nested more than {MAX_DEPTH} levels deep; is there a cycle?"
                )));
            }
            if t.to_pointer() == markers.null.to_pointer() {
                return Ok(Raw::Null);
            }
            let lua_err = |e: mlua::Error| fail(e.to_string());
            if let Some(prio) = t.raw_get::<Option<i64>>(&markers.prio).map_err(lua_err)? {
                let inner: LuaValue = t.raw_get("value").map_err(lua_err)?;
                let prio = i32::try_from(prio).map_err(|_| fail(format!("priority {prio} is out of range")))?;
                return Ok(Raw::Prio(prio, Box::new(to_raw_at(markers, &inner, path, depth + 1)?)));
            }
            let is_marked = |key: &Table| t.raw_get::<LuaValue>(key).map(|v| !v.is_nil()).unwrap_or(false);
            if is_marked(&markers.option) {
                return Err(fail(
                    "archix.option{...} declarations belong under `options`, not in config".into(),
                ));
            }
            if is_marked(&markers.ty) {
                return Err(fail("an archix type can't be used as a config value".into()));
            }
            let mut items: BTreeMap<i64, LuaValue> = BTreeMap::new();
            let mut fields: BTreeMap<String, LuaValue> = BTreeMap::new();
            for pair in t.pairs::<LuaValue, LuaValue>() {
                let (key, value) = pair.map_err(lua_err)?;
                match key {
                    LuaValue::Integer(i) => {
                        items.insert(i, value);
                    }
                    LuaValue::String(s) => {
                        let key = s.to_str().map_err(|_| fail("table key is not valid UTF-8".into()))?;
                        fields.insert(key.to_string(), value);
                    }
                    other => {
                        return Err(fail(format!(
                            "{} keys aren't allowed in config tables",
                            other.type_name()
                        )));
                    }
                }
            }
            match (items.is_empty(), fields.is_empty()) {
                (true, true) => Ok(Raw::Empty),
                (false, false) => Err(fail("table mixes list items and named fields".into())),
                (false, true) => {
                    if items.keys().copied().ne(1..=items.len() as i64) {
                        return Err(fail("list has holes or non-sequential indices".into()));
                    }
                    let list = items
                        .values()
                        .enumerate()
                        .map(|(i, v)| to_raw_at(markers, v, &item_path(path, i + 1), depth + 1))
                        .collect::<Result<_, _>>()?;
                    Ok(Raw::List(list))
                }
                (true, false) => {
                    let map = fields
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), to_raw_at(markers, v, &options::join(path, k), depth + 1)?)))
                        .collect::<Result<_, Diagnostic>>()?;
                    Ok(Raw::Map(map))
                }
            }
        }
        LuaValue::Nil => Err(fail("value is nil".into())),
        other => Err(fail(format!("a {} can't be a config value", other.type_name()))),
    }
}

/// Reads an `options` table into `into`, declaring every `archix.option{...}` found in it.
pub fn declare_options(
    markers: &Markers,
    table: &Table,
    prefix: &[String],
    source: &Rc<str>,
    into: &mut Children,
    diags: &mut Vec<Diagnostic>,
) {
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, value) = match pair {
            Ok(pair) => pair,
            Err(e) => {
                diags.push(Diagnostic::new(&prefix.join("."), format!("{source}: {e}")));
                continue;
            }
        };
        let Some(key) = key.as_string().and_then(|s| s.to_str().ok()).map(|s| s.to_string()) else {
            diags.push(Diagnostic::new(
                &prefix.join("."),
                format!("{source}: option names must be strings"),
            ));
            continue;
        };
        let mut path = prefix.to_vec();
        path.push(key);
        let dotted = path.join(".");
        match value {
            LuaValue::Table(t) if t.raw_get::<LuaValue>(&markers.option).is_ok_and(|v| !v.is_nil()) => {
                match parse_decl(markers, &t, &dotted, source, diags) {
                    Some(decl) => {
                        if let Err(diag) = options::declare(into, &path, decl) {
                            diags.push(diag);
                        }
                    }
                    None => continue,
                }
            }
            LuaValue::Table(t) => declare_options(markers, &t, &path, source, into, diags),
            _ => diags.push(Diagnostic::new(
                &dotted,
                format!("{source}: expected archix.option{{...}} or a table of options"),
            )),
        }
    }
}

fn parse_decl(markers: &Markers, t: &Table, path: &str, source: &Rc<str>, diags: &mut Vec<Diagnostic>) -> Option<Decl> {
    let ty_table: Table = t.raw_get("type").ok()?;
    let ty = parse_type(markers, &ty_table, path, source, diags)?;
    let description: Option<String> = match t.raw_get("description") {
        Ok(d) => d,
        Err(_) => {
            diags.push(Diagnostic::new(path, format!("{source}: description must be a string")));
            None
        }
    };
    let default_value: LuaValue = t.raw_get("default").unwrap_or(LuaValue::Nil);
    let default = if default_value.is_nil() {
        None
    } else {
        let raw = match to_raw(markers, &default_value, path) {
            Ok(raw) => raw,
            Err(diag) => {
                diags.push(diag);
                return None;
            }
        };
        let check_source: Rc<str> = format!("the default declared in {source}").into();
        if let Err(errors) = options::check(path, &ty, Def::new(raw.clone(), check_source)) {
            diags.extend(errors);
            return None;
        }
        Some(raw)
    };
    Some(Decl {
        ty,
        default,
        description,
        source: source.clone(),
    })
}

fn parse_type(markers: &Markers, t: &Table, path: &str, source: &Rc<str>, diags: &mut Vec<Diagnostic>) -> Option<Type> {
    let kind: String = t.raw_get(&markers.ty).ok()?;
    let elem = |diags: &mut Vec<Diagnostic>| -> Option<Box<Type>> {
        let elem: Table = t.raw_get("elem").ok()?;
        parse_type(markers, &elem, path, source, diags).map(Box::new)
    };
    Some(match kind.as_str() {
        "bool" => Type::Bool,
        "int" => Type::Int,
        "float" => Type::Float,
        "str" => Type::Str,
        "path" => Type::Path,
        "lines" => Type::Lines,
        "any" => Type::Any,
        "enum" => Type::Enum(t.raw_get("values").ok()?),
        "list" => Type::List(elem(diags)?),
        "attrs" => Type::Attrs(elem(diags)?),
        "nullable" => Type::Nullable(elem(diags)?),
        "submodule" => {
            let options: Table = t.raw_get("options").ok()?;
            let mut children = Children::new();
            declare_options(markers, &options, &[], source, &mut children, diags);
            Type::Submodule(Rc::new(children))
        }
        other => {
            diags.push(Diagnostic::new(path, format!("{source}: unknown type kind {other:?}")));
            return None;
        }
    })
}

/// Builds the read-only view of the merged config that `config = function(cfg)` receives.
pub fn cfg_table(lua: &Lua, markers: &Markers, root: &Rc<Children>, value: &Value) -> mlua::Result<Table> {
    let map = match value {
        Value::Map(map) => Some(map),
        _ => None,
    };
    group_table(lua, &markers.null, root.clone(), String::new(), map)
}

fn group_table(
    lua: &Lua,
    null: &Table,
    children: Rc<Children>,
    path: String,
    value: Option<&BTreeMap<String, Value>>,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let mut nulls = HashSet::new();
    for (key, value) in value.into_iter().flatten() {
        let sub = options::join(&path, key);
        match children.get(key) {
            Some(Node::Group(group)) => {
                let map = match value {
                    Value::Map(map) => Some(map),
                    _ => None,
                };
                table.raw_set(key.as_str(), group_table(lua, null, group.clone(), sub, map)?)?;
            }
            Some(Node::Opt(_)) if *value == Value::Null => {
                nulls.insert(key.clone());
            }
            Some(Node::Opt(decl)) => table.raw_set(key.as_str(), typed_to_lua(lua, null, &decl.ty, value, sub)?)?,
            None => {}
        }
    }

    let null = null.clone();
    let index = lua.create_function(move |lua, (_, key): (LuaValue, LuaValue)| {
        let Some(key) = key.as_string().and_then(|s| s.to_str().ok()).map(|s| s.to_string()) else {
            return Ok(LuaValue::Nil);
        };
        let sub = options::join(&path, &key);
        match children.get(&key) {
            Some(Node::Group(group)) => Ok(LuaValue::Table(group_table(lua, &null, group.clone(), sub, None)?)),
            Some(Node::Opt(_)) => {
                if !nulls.contains(&key) {
                    let mut unset = lua.app_data_mut::<UnsetReads>().expect("unset reads state");
                    if let Some(module) = unset.current.clone() {
                        unset.reads.push((sub, module));
                    }
                }
                Ok(LuaValue::Nil)
            }
            None => Err(mlua::Error::runtime(format!(
                "option `{sub}` does not exist{}",
                options::did_you_mean(&path, &key, &children)
            ))),
        }
    })?;
    let newindex = lua.create_function(|_, ()| -> mlua::Result<()> {
        Err(mlua::Error::runtime(
            "cfg is read-only; return definitions from the module instead",
        ))
    })?;
    let meta = lua.create_table()?;
    meta.raw_set("__index", index)?;
    meta.raw_set("__newindex", newindex)?;
    table.set_metatable(Some(meta))?;
    Ok(table)
}

fn typed_to_lua(lua: &Lua, null: &Table, ty: &Type, value: &Value, path: String) -> mlua::Result<LuaValue> {
    match (ty, value) {
        (Type::Submodule(children), Value::Map(map)) => Ok(LuaValue::Table(group_table(
            lua,
            null,
            children.clone(),
            path,
            Some(map),
        )?)),
        (Type::List(elem), Value::List(items)) => {
            let table = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                table.raw_set(i + 1, typed_to_lua(lua, null, elem, item, item_path(&path, i + 1))?)?;
            }
            Ok(LuaValue::Table(table))
        }
        (Type::Attrs(elem), Value::Map(map)) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.raw_set(
                    key.as_str(),
                    typed_to_lua(lua, null, elem, item, options::join(&path, key))?,
                )?;
            }
            Ok(LuaValue::Table(table))
        }
        (Type::Nullable(elem), value) if *value != Value::Null => typed_to_lua(lua, null, elem, value, path),
        (_, value) => plain_to_lua(lua, null, value),
    }
}

fn plain_to_lua(lua: &Lua, null: &Table, value: &Value) -> mlua::Result<LuaValue> {
    Ok(match value {
        Value::Null => LuaValue::Table(null.clone()),
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Int(i) => LuaValue::Integer(*i),
        Value::Float(f) => LuaValue::Number(*f),
        Value::Str(s) => LuaValue::String(lua.create_string(s)?),
        Value::List(items) => {
            let table = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                table.raw_set(i + 1, plain_to_lua(lua, null, item)?)?;
            }
            LuaValue::Table(table)
        }
        Value::Map(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.raw_set(key.as_str(), plain_to_lua(lua, null, item)?)?;
            }
            LuaValue::Table(table)
        }
    })
}
