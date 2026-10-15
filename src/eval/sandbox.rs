use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Lua, LuaOptions, StdLib, Table, Value as LuaValue, Variadic};

use crate::options::{PRIO_DEFAULT, PRIO_FORCE};

/// Private keys the prelude uses to tag option declarations, types and priorities.
pub struct Markers {
    pub option: Table,
    pub ty: Table,
    pub prio: Table,
    pub null: Table,
}

/// Modules shipped inside the binary, loaded before init.lua and reachable as `archix.<name>`.
pub const BUILTIN_MODULES: &[(&str, &str)] = &[
    ("packages", include_str!("modules/packages.lua")),
    ("generations", include_str!("modules/generations.lua")),
    ("aur", include_str!("modules/aur.lua")),
    ("system", include_str!("modules/system.lua")),
    ("hooks", include_str!("modules/hooks.lua")),
    ("environment", include_str!("modules/environment.lua")),
    ("firewall", include_str!("modules/firewall.lua")),
    ("boot", include_str!("modules/boot.lua")),
    ("appimages", include_str!("modules/appimages.lua")),
    ("flatpak", include_str!("modules/flatpak.lua")),
    ("home", include_str!("modules/home.lua")),
    ("home_systemd", include_str!("modules/home_systemd.lua")),
    ("mime_apps", include_str!("modules/programs/mime_apps.lua")),
    ("programs.fastfetch", include_str!("modules/programs/fastfetch.lua")),
    ("programs.git", include_str!("modules/programs/git.lua")),
    ("programs.kitty", include_str!("modules/programs/kitty.lua")),
    ("programs.starship", include_str!("modules/programs/starship.lua")),
    ("programs.thin", include_str!("modules/programs/thin.lua")),
    ("programs.zsh", include_str!("modules/programs/zsh.lua")),
];

pub struct Sandbox {
    pub lua: Lua,
    pub markers: Markers,
}

struct Modules {
    root: PathBuf,
    cache: HashMap<String, (LuaValue, Rc<str>)>,
    loading: Vec<String>,
}

/// Options a module read that had no value, recorded by the strict cfg tables.
#[derive(Default)]
pub struct UnsetReads {
    pub current: Option<Rc<str>>,
    pub reads: Vec<(String, Rc<str>)>,
}

impl Sandbox {
    pub fn new(root: &Path, hostname: &str) -> mlua::Result<Self> {
        let libs = StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH | StdLib::COROUTINE;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        let globals = lua.globals();
        for name in ["dofile", "loadfile", "load", "collectgarbage"] {
            globals.raw_set(name, LuaValue::Nil)?;
        }
        let math: Table = globals.raw_get("math")?;
        math.raw_set("random", LuaValue::Nil)?;
        math.raw_set("randomseed", LuaValue::Nil)?;
        globals.raw_set("print", lua.create_function(trace)?)?;
        globals.raw_set(
            "require",
            lua.create_function(|lua, name: String| require(lua, &name).map(|(v, _)| v))?,
        )?;

        let markers: Table = lua
            .load(include_str!("prelude.lua"))
            .set_name("@archix/prelude.lua")
            .call((PRIO_FORCE, PRIO_DEFAULT))?;
        let markers = Markers {
            option: markers.raw_get("option")?,
            ty: markers.raw_get("type")?,
            prio: markers.raw_get("prio")?,
            null: markers.raw_get("null")?,
        };

        lua.load(include_str!("formats.lua"))
            .set_name("@archix/formats.lua")
            .exec()?;
        let archix: Table = globals.raw_get("archix")?;
        archix.raw_set("hostname", hostname)?;
        archix.raw_set("read_file", lua.create_function(read_file)?)?;

        lua.set_app_data(Modules {
            root: root.to_path_buf(),
            cache: HashMap::new(),
            loading: Vec::new(),
        });
        lua.set_app_data(UnsetReads::default());
        Ok(Sandbox { lua, markers })
    }

    pub fn load_init(&self) -> mlua::Result<(LuaValue, Rc<str>)> {
        let root = self.lua.app_data_ref::<Modules>().expect("modules state").root.clone();
        let code = fs::read_to_string(root.join("init.lua"))
            .map_err(|e| mlua::Error::runtime(format!("reading init.lua: {e}")))?;
        let value = run_chunk(&self.lua, code, "init.lua")?;
        Ok((value, "init.lua".into()))
    }

    pub fn require(&self, name: &str) -> mlua::Result<(LuaValue, Rc<str>)> {
        require(&self.lua, name)
    }
}

fn trace(_: &Lua, args: Variadic<LuaValue>) -> mlua::Result<()> {
    let parts: Vec<String> = args
        .iter()
        .map(|v| v.to_string().unwrap_or_else(|_| v.type_name().into()))
        .collect();
    eprintln!("trace: {}", parts.join("\t"));
    Ok(())
}

fn require(lua: &Lua, name: &str) -> mlua::Result<(LuaValue, Rc<str>)> {
    let valid = !name.is_empty()
        && name
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    if !valid {
        return Err(mlua::Error::runtime(format!(
            "invalid module name {name:?}; use dotted names like \"hosts.desktop\""
        )));
    }
    let root = {
        let modules = lua.app_data_ref::<Modules>().expect("modules state");
        if let Some(hit) = modules.cache.get(name) {
            return Ok(hit.clone());
        }
        if modules.loading.iter().any(|n| n == name) {
            let chain = modules.loading.join(" -> ");
            return Err(mlua::Error::runtime(format!("require cycle: {chain} -> {name}")));
        }
        modules.root.clone()
    };
    let (source, code) = match name.strip_prefix("archix.") {
        Some(builtin) => match BUILTIN_MODULES.iter().find(|(n, _)| *n == builtin) {
            Some((n, code)) => (format!("<archix>/{n}.lua"), code.to_string()),
            None => return Err(mlua::Error::runtime(format!("archix has no built-in module {name:?}"))),
        },
        None => {
            let rel = name.replace('.', "/");
            let candidates = [format!("lua/{rel}.lua"), format!("lua/{rel}/init.lua")];
            let Some(source) = candidates.iter().find(|c| root.join(c).is_file()) else {
                return Err(mlua::Error::runtime(format!(
                    "module {name:?} not found (looked for {} and {})",
                    candidates[0], candidates[1]
                )));
            };
            let code = fs::read_to_string(root.join(source))
                .map_err(|e| mlua::Error::runtime(format!("reading {source}: {e}")))?;
            (source.clone(), code)
        }
    };

    lua.app_data_mut::<Modules>()
        .expect("modules state")
        .loading
        .push(name.to_string());
    let result = run_chunk(lua, code, &source);
    lua.app_data_mut::<Modules>().expect("modules state").loading.pop();

    let entry = (result?, Rc::from(source.as_str()));
    lua.app_data_mut::<Modules>()
        .expect("modules state")
        .cache
        .insert(name.to_string(), entry.clone());
    Ok(entry)
}

fn run_chunk(lua: &Lua, code: String, display: &str) -> mlua::Result<LuaValue> {
    lua.load(code)
        .set_name(format!("@{display}"))
        .set_mode(ChunkMode::Text)
        .eval()
}

fn read_file(lua: &Lua, path: String) -> mlua::Result<String> {
    let fail = |msg: String| mlua::Error::runtime(format!("archix.read_file({path:?}): {msg}"));
    if Path::new(&path).is_absolute() {
        return Err(fail("path must be relative to the config directory".into()));
    }
    let root = lua.app_data_ref::<Modules>().expect("modules state").root.clone();
    let full = root.join(&path).canonicalize().map_err(|e| fail(e.to_string()))?;
    if !full.starts_with(&root) {
        return Err(fail("path leaves the config directory".into()));
    }
    fs::read_to_string(full).map_err(|e| fail(e.to_string()))
}

/// The `file:line` of the first Lua frame in a traceback, i.e. the config code that called into Rust.
fn caller_location(traceback: &str) -> Option<&str> {
    traceback
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("[C]") && !line.starts_with("archix/") && !line.starts_with("<archix>/"))
        .find_map(|line| {
            let (file, rest) = line.split_once(':')?;
            let (number, _) = rest.split_once(':')?;
            number.parse::<u32>().ok()?;
            Some(&line[..file.len() + 1 + number.len()])
        })
}

/// The message of a Lua error without mlua's wrapping and stack traceback.
pub fn lua_message(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { cause, traceback } => {
            let message = lua_message(cause);
            match caller_location(traceback) {
                Some(location) if !message.starts_with(location) => format!("{location}: {message}"),
                _ => message,
            }
        }
        mlua::Error::RuntimeError(msg) => msg.split("\nstack traceback:").next().unwrap_or(msg).to_string(),
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        mlua::Error::WithContext { cause, .. } => lua_message(cause),
        other => other.to_string(),
    }
}
