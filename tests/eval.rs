use std::fs;
use std::path::Path;

use archix::eval::{EvalError, Evaluation, Input, evaluate};
use archix::value::Value;
use serde_json::json;
use tempfile::TempDir;

const TERMY: &str = r#"
local t = archix.types
return {
  options = {
    programs = { termy = {
      enable = archix.option { type = t.bool, default = false, description = "Install and configure termy" },
      font = { size = archix.option { type = t.int, default = 12 } },
    } },
  },
  config = function(cfg)
    if not cfg.programs.termy.enable then return {} end
    return { packages = { "termy" } }
  end,
}
"#;

fn config(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn eval_at(dir: &Path, host: &str) -> Result<Evaluation, EvalError> {
    evaluate(&Input {
        config_dir: dir.to_path_buf(),
        hostname: host.to_string(),
        ..Default::default()
    })
}

fn eval_ok(files: &[(&str, &str)]) -> serde_json::Value {
    let dir = config(files);
    match eval_at(dir.path(), "desktop") {
        Ok(evaluation) => serde_json::to_value(&evaluation.config).unwrap(),
        Err(e) => panic!("evaluation failed:\n{e}"),
    }
}

fn eval_err(files: &[(&str, &str)]) -> String {
    let dir = config(files);
    match eval_at(dir.path(), "desktop") {
        Ok(evaluation) => panic!(
            "expected an error, got {}",
            serde_json::to_string(&evaluation.config).unwrap()
        ),
        Err(e) => e.to_string(),
    }
}

#[test]
fn host_file_force_overrides_shared_module() {
    let out = eval_ok(&[
        (
            "init.lua",
            r#"return { imports = { "modules.termy", "modules.common", "hosts." .. archix.hostname } }"#,
        ),
        ("lua/modules/termy.lua", TERMY),
        (
            "lua/modules/common.lua",
            r#"return { programs = { termy = { enable = true, font = { size = 14 } } } }"#,
        ),
        (
            "lua/hosts/desktop.lua",
            r#"return { programs = { termy = { font = { size = archix.force(11) } } }, packages = { "git" } }"#,
        ),
    ]);
    assert_eq!(
        out["programs"]["termy"],
        json!({ "enable": true, "font": { "size": 11 } })
    );
    assert_eq!(out["packages"], json!(["termy", "git"]));
}

#[test]
fn hostname_selects_the_host_file() {
    let dir = config(&[
        (
            "init.lua",
            r#"return { imports = { "modules.termy", "hosts." .. archix.hostname } }"#,
        ),
        ("lua/modules/termy.lua", TERMY),
        ("lua/hosts/desktop.lua", r#"return { packages = { "steam" } }"#),
        ("lua/hosts/laptop.lua", r#"return { packages = { "tlp" } }"#),
    ]);
    let laptop = eval_at(dir.path(), "laptop").unwrap();
    let Value::Map(map) = &laptop.config else { panic!() };
    assert_eq!(map["packages"], Value::List(vec![Value::Str("tlp".into())]));
}

#[test]
fn function_modules_settle_even_when_read_before_defined() {
    let out = eval_ok(&[
        (
            "init.lua",
            r#"return { imports = { "modules.termy", "modules.desktop", "modules.reader" } }"#,
        ),
        ("lua/modules/termy.lua", TERMY),
        (
            "lua/modules/reader.lua",
            r#"
            return {
              options = { summary = archix.option { type = archix.types.str, default = "" } },
              config = function(cfg) return { summary = table.concat(cfg.packages, ",") } end,
            }"#,
        ),
        (
            "lua/modules/desktop.lua",
            r#"return function(cfg) return { programs = { termy = { enable = true } } } end"#,
        ),
    ]);
    assert_eq!(out["summary"], json!("termy"));
}

#[test]
fn typo_in_config_suggests_the_real_option() {
    let err = eval_err(&[
        (
            "init.lua",
            r#"return { imports = { "modules.termy" }, config = { programs = { termi = { enable = true } } } }"#,
        ),
        ("lua/modules/termy.lua", TERMY),
    ]);
    assert!(
        err.contains("`programs.termi`: option does not exist (set in init.lua); did you mean `programs.termy`?"),
        "{err}"
    );
}

#[test]
fn typo_when_reading_cfg_is_an_error() {
    let err = eval_err(&[
        ("init.lua", r#"return { imports = { "modules.termy", "modules.bad" } }"#),
        ("lua/modules/termy.lua", TERMY),
        (
            "lua/modules/bad.lua",
            r#"return function(cfg) local _ = cfg.programs.termi.enable end"#,
        ),
    ]);
    assert!(
        err.contains("option `programs.termi` does not exist; did you mean `programs.termy`?"),
        "{err}"
    );
}

#[test]
fn reading_an_unset_option_without_default_is_an_error() {
    let err = eval_err(&[(
        "init.lua",
        r#"return {
          options = { name = archix.option { type = archix.types.str }, greeting = archix.option { type = archix.types.str } },
          config = function(cfg) return { greeting = "hi " .. tostring(cfg.name) } end,
        }"#,
    )]);
    assert!(
        err.contains("`name`: read by init.lua but has no value and no default"),
        "{err}"
    );
}

#[test]
fn type_errors_point_at_the_list_item() {
    let err = eval_err(&[
        (
            "init.lua",
            r#"return { imports = { "modules.termy" }, config = { packages = { "git", 3 } } }"#,
        ),
        ("lua/modules/termy.lua", TERMY),
    ]);
    assert!(
        err.contains("`packages[2]`: expected str, got integer 3 in init.lua"),
        "{err}"
    );
}

#[test]
fn bad_default_is_reported_at_declaration() {
    let err = eval_err(&[(
        "init.lua",
        r#"return { options = { size = archix.option { type = archix.types.int, default = "big" } } }"#,
    )]);
    assert!(
        err.contains("`size`: expected int, got string \"big\" in the default declared in init.lua"),
        "{err}"
    );
}

#[test]
fn sandbox_has_no_io_or_os() {
    let err = eval_err(&[("init.lua", r#"return function() return io.open("/etc/passwd") end"#)]);
    assert!(err.contains("io"), "{err}");
    let err = eval_err(&[("init.lua", r#"return function() return os.getenv("HOME") end"#)]);
    assert!(err.contains("os"), "{err}");
}

#[test]
fn read_file_stays_inside_the_config_dir() {
    let out = eval_ok(&[
        (
            "init.lua",
            r#"return {
              options = { motd = archix.option { type = archix.types.str } },
              config = { motd = archix.read_file("files/motd") },
            }"#,
        ),
        ("files/motd", "hello"),
    ]);
    assert_eq!(out["motd"], json!("hello"));
    let err = eval_err(&[("init.lua", r#"return { x = archix.read_file("../../etc/hostname") }"#)]);
    assert!(
        err.contains("path leaves the config directory") || err.contains("No such file"),
        "{err}"
    );
    let err = eval_err(&[("init.lua", r#"return { x = archix.read_file("/etc/hostname") }"#)]);
    assert!(err.contains("path must be relative"), "{err}");
}

#[test]
fn require_reports_cycles_and_missing_modules() {
    let err = eval_err(&[
        ("init.lua", r#"require("a") return {}"#),
        ("lua/a.lua", r#"require("b") return {}"#),
        ("lua/b.lua", r#"require("a") return {}"#),
    ]);
    assert!(err.contains("require cycle: a -> b -> a"), "{err}");
    let err = eval_err(&[("init.lua", r#"return { imports = { "hosts.nope" } }"#)]);
    assert!(
        err.contains("module \"hosts.nope\" not found (looked for lua/hosts/nope.lua and lua/hosts/nope/init.lua)"),
        "{err}"
    );
}

#[test]
fn pairs_iterates_in_sorted_order() {
    let out = eval_ok(&[(
        "init.lua",
        r#"
        local t = archix.types
        return {
          options = {
            people = archix.option { type = t.attrs_of(t.int), default = {} },
            names = archix.option { type = t.list_of(t.str), default = {} },
          },
          config = function(cfg)
            local names = {}
            for name in pairs(cfg.people) do names[#names + 1] = name end
            return { people = { zed = 1, amy = 2, kim = 3 }, names = names }
          end,
        }"#,
    )]);
    assert_eq!(out["names"], json!(["amy", "kim", "zed"]));
}

#[test]
fn self_feeding_config_fails_to_settle() {
    let err = eval_err(&[(
        "init.lua",
        r#"return {
          options = { counter = archix.option { type = archix.types.int, default = 0 } },
          config = function(cfg) return { counter = cfg.counter + 1 } end,
        }"#,
    )]);
    assert!(err.contains("did not settle") && err.contains("`counter`"), "{err}");
}

#[test]
fn cfg_is_read_only() {
    let err = eval_err(&[
        ("init.lua", r#"return { imports = { "modules.termy", "modules.bad" } }"#),
        ("lua/modules/termy.lua", TERMY),
        (
            "lua/modules/bad.lua",
            r#"return function(cfg) cfg.programs.newthing = 1 end"#,
        ),
    ]);
    assert!(err.contains("cfg is read-only"), "{err}");
}

#[test]
fn stray_module_fields_are_rejected() {
    let err = eval_err(&[("init.lua", r#"return { imports = {}, packages = { "git" } }"#)]);
    assert!(err.contains("unknown module field `packages`"), "{err}");
}

#[test]
fn null_and_nullable() {
    let out = eval_ok(&[(
        "init.lua",
        r#"
        local t = archix.types
        return {
          options = { editor = archix.option { type = t.nullable(t.str), default = "nvim" }, uses_null = archix.option { type = t.bool, default = false } },
          config = function(cfg) return { editor = archix.null, uses_null = cfg.editor == nil } end,
        }"#,
    )]);
    assert_eq!(out["editor"], json!(null));
    assert_eq!(out["uses_null"], json!(true));
}
