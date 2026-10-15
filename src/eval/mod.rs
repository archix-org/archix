mod convert;
mod sandbox;

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Function, Table, Value as LuaValue};

use crate::options::{self, Children, Def, Diagnostic};
use crate::value::{Raw, Value};
use sandbox::{BUILTIN_MODULES, Sandbox, UnsetReads, lua_message};

const MAX_ROUNDS: usize = 32;
const MODULE_KEYS: [&str; 3] = ["imports", "options", "config"];

pub struct Input {
    pub config_dir: PathBuf,
    pub hostname: String,
}

pub struct Evaluation {
    pub config: Value,
    pub options: Rc<Children>,
}

#[derive(Debug)]
pub struct EvalError(pub Vec<Diagnostic>);

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines: Vec<String> = self.0.iter().map(|d| format!("error: {d}")).collect();
        f.write_str(&lines.join("\n"))
    }
}

impl std::error::Error for EvalError {}

impl EvalError {
    fn one(message: String) -> Self {
        EvalError(vec![Diagnostic::new("", message)])
    }
}

pub fn evaluate(input: &Input) -> Result<Evaluation, EvalError> {
    let root = input
        .config_dir
        .canonicalize()
        .map_err(|e| EvalError::one(format!("config directory {}: {e}", input.config_dir.display())))?;
    if !root.join("init.lua").is_file() {
        return Err(EvalError::one(format!("no init.lua in {}", root.display())));
    }
    let sandbox = Sandbox::new(&root, &input.hostname).map_err(|e| EvalError::one(lua_message(&e)))?;

    let mut loader = Loader::new(&sandbox);
    for (name, _) in BUILTIN_MODULES {
        loader.import(&format!("archix.{name}"), "<archix>");
    }
    match sandbox.load_init() {
        Ok((value, source)) => loader.add(value, source),
        Err(e) => loader.fail(lua_message(&e)),
    }
    if crate::installed::default_dir(&root, &input.hostname)
        .join(crate::installed::FILE)
        .is_file()
    {
        loader.import(&format!("hosts.{}.installed", input.hostname), "<archix>");
    }
    if !loader.diags.is_empty() {
        return Err(EvalError(loader.diags));
    }
    let options = Rc::new(loader.options);
    let (config, diags) = fixpoint(&sandbox, &options, &loader.modules)?;
    if diags.is_empty() {
        Ok(Evaluation { config, options })
    } else {
        Err(EvalError(diags))
    }
}

enum ModuleConfig {
    Static(Raw),
    Function(Function),
}

struct Module {
    source: Rc<str>,
    config: ModuleConfig,
}

struct Loader<'a> {
    sandbox: &'a Sandbox,
    seen: HashSet<String>,
    modules: Vec<Module>,
    options: Children,
    diags: Vec<Diagnostic>,
}

impl<'a> Loader<'a> {
    fn new(sandbox: &'a Sandbox) -> Self {
        Loader {
            sandbox,
            seen: HashSet::new(),
            modules: Vec::new(),
            options: Children::new(),
            diags: Vec::new(),
        }
    }

    fn fail(&mut self, message: String) {
        self.diags.push(Diagnostic::new("", message));
    }

    fn import(&mut self, name: &str, from: &str) {
        if !self.seen.insert(name.to_string()) {
            return;
        }
        match self.sandbox.require(name) {
            Ok((value, source)) => self.add(value, source),
            Err(e) => self.fail(format!("{from}: importing {name:?}: {}", lua_message(&e))),
        }
    }

    fn add(&mut self, value: LuaValue, source: Rc<str>) {
        match value {
            LuaValue::Function(f) => self.modules.push(Module {
                source,
                config: ModuleConfig::Function(f),
            }),
            LuaValue::Table(t) => self.add_table(t, source),
            LuaValue::Nil => self.fail(format!(
                "{source}: returned nothing; a module must return a table or a function"
            )),
            other => self.fail(format!(
                "{source}: returned a {}; a module must return a table or a function",
                other.type_name()
            )),
        }
    }

    fn add_table(&mut self, t: Table, source: Rc<str>) {
        let has = |key: &str| t.raw_get::<LuaValue>(key).is_ok_and(|v| !v.is_nil());
        if !MODULE_KEYS.iter().any(|key| has(key)) {
            self.add_static(LuaValue::Table(t), source);
            return;
        }
        for pair in t.pairs::<LuaValue, LuaValue>() {
            let Ok((key, _)) = pair else { continue };
            let name = key.to_string().unwrap_or_default();
            if !MODULE_KEYS.contains(&name.as_str()) {
                self.fail(format!(
                    "{source}: unknown module field `{name}`; a module with imports, options or config can't have other fields (put settings under config)"
                ));
            }
        }

        match t.raw_get::<LuaValue>("config") {
            Ok(LuaValue::Function(f)) => self.modules.push(Module {
                source: source.clone(),
                config: ModuleConfig::Function(f),
            }),
            Ok(LuaValue::Nil) => {}
            Ok(value @ LuaValue::Table(_)) => self.add_static(value, source.clone()),
            Ok(other) => self.fail(format!(
                "{source}: config must be a table or a function, got a {}",
                other.type_name()
            )),
            Err(e) => self.fail(format!("{source}: {}", lua_message(&e))),
        }

        match t.raw_get::<LuaValue>("options") {
            Ok(LuaValue::Table(options)) => convert::declare_options(
                &self.sandbox.markers,
                &options,
                &[],
                &source,
                &mut self.options,
                &mut self.diags,
            ),
            Ok(LuaValue::Nil) => {}
            Ok(other) => self.fail(format!(
                "{source}: options must be a table, got a {}",
                other.type_name()
            )),
            Err(e) => self.fail(format!("{source}: {}", lua_message(&e))),
        }

        match t.raw_get::<LuaValue>("imports") {
            Ok(LuaValue::Table(imports)) => {
                for (i, item) in imports.sequence_values::<LuaValue>().enumerate() {
                    match item {
                        Ok(LuaValue::String(name)) => {
                            let name = name.to_string_lossy();
                            self.import(&name, &source);
                        }
                        Ok(value @ (LuaValue::Table(_) | LuaValue::Function(_))) => {
                            self.add(value, format!("{source} (import #{})", i + 1).into())
                        }
                        Ok(other) => self.fail(format!(
                            "{source}: imports must be module names or modules, got a {}",
                            other.type_name()
                        )),
                        Err(e) => self.fail(format!("{source}: {}", lua_message(&e))),
                    }
                }
            }
            Ok(LuaValue::Nil) => {}
            Ok(other) => self.fail(format!("{source}: imports must be a list, got a {}", other.type_name())),
            Err(e) => self.fail(format!("{source}: {}", lua_message(&e))),
        }
    }

    fn add_static(&mut self, value: LuaValue, source: Rc<str>) {
        match convert::to_raw(&self.sandbox.markers, &value, "") {
            Ok(raw) => self.modules.push(Module {
                source,
                config: ModuleConfig::Static(raw),
            }),
            Err(diag) => self.diags.push(Diagnostic {
                message: format!("{} (in {source})", diag.message),
                ..diag
            }),
        }
    }
}

/// Re-runs function modules against the merged config until their output stops changing.
fn fixpoint(
    sandbox: &Sandbox,
    options: &Rc<Children>,
    modules: &[Module],
) -> Result<(Value, Vec<Diagnostic>), EvalError> {
    let statics = modules.iter().filter_map(|m| match &m.config {
        ModuleConfig::Static(raw) => Some(Def::new(raw.clone(), m.source.clone())),
        ModuleConfig::Function(_) => None,
    });
    let (mut previous, diags) = options::merge(options, statics.collect());
    if !modules.iter().any(|m| matches!(m.config, ModuleConfig::Function(_))) {
        return Ok((previous, diags));
    }

    for _ in 0..MAX_ROUNDS {
        let (state, diags) = round(sandbox, options, modules, &previous);
        if state == previous {
            return Ok((state, diags));
        }
        previous = state;
    }

    let (last, _) = round(sandbox, options, modules, &previous);
    let mut changing = BTreeSet::new();
    changed_paths(&previous, &last, "", &mut changing);
    let changing: Vec<String> = changing.into_iter().map(|p| format!("`{p}`")).collect();
    Err(EvalError::one(format!(
        "config did not settle after {MAX_ROUNDS} rounds; modules keep changing {}",
        changing.join(", ")
    )))
}

fn round(sandbox: &Sandbox, options: &Rc<Children>, modules: &[Module], previous: &Value) -> (Value, Vec<Diagnostic>) {
    let lua = &sandbox.lua;
    let mut defs = Vec::with_capacity(modules.len());
    let mut diags = Vec::new();
    lua.app_data_mut::<UnsetReads>()
        .expect("unset reads state")
        .reads
        .clear();

    for module in modules {
        let f = match &module.config {
            ModuleConfig::Static(raw) => {
                defs.push(Def::new(raw.clone(), module.source.clone()));
                continue;
            }
            ModuleConfig::Function(f) => f,
        };
        lua.app_data_mut::<UnsetReads>().expect("unset reads state").current = Some(module.source.clone());
        let result =
            convert::cfg_table(lua, &sandbox.markers, options, previous).and_then(|cfg| f.call::<LuaValue>(cfg));
        match result {
            Ok(LuaValue::Nil) => {}
            Ok(value) => match convert::to_raw(&sandbox.markers, &value, "") {
                Ok(raw) => defs.push(Def::new(raw, module.source.clone())),
                Err(diag) => diags.push(Diagnostic {
                    message: format!("{} (returned by {})", diag.message, module.source),
                    ..diag
                }),
            },
            Err(e) => {
                let message = lua_message(&e);
                let message = if message.starts_with(&*module.source) {
                    message
                } else {
                    format!("{}: {message}", module.source)
                };
                diags.push(Diagnostic::new("", message));
            }
        }
    }

    let (state, merge_diags) = options::merge(options, defs);
    let mut unset = lua.app_data_mut::<UnsetReads>().expect("unset reads state");
    unset.current = None;
    for (path, module) in unset.reads.drain(..) {
        let diag = Diagnostic::new(&path, format!("read by {module} but has no value and no default"));
        if !diags.contains(&diag) {
            diags.push(diag);
        }
    }
    diags.extend(merge_diags);
    (state, diags)
}

fn changed_paths(a: &Value, b: &Value, path: &str, out: &mut BTreeSet<String>) {
    match (a, b) {
        (Value::Map(a), Value::Map(b)) => {
            for key in a.keys().chain(b.keys()) {
                let sub = options::join(path, key);
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => changed_paths(x, y, &sub, out),
                    _ => {
                        out.insert(sub);
                    }
                }
            }
        }
        _ if a != b => {
            out.insert(if path.is_empty() {
                "<root>".into()
            } else {
                path.to_string()
            });
        }
        _ => {}
    }
}
