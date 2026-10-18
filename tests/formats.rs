use std::fs;

use archix::eval::{Input, evaluate};
use archix::value::Value;

/// Evaluates `expr` (a Lua expression producing a string) in a throwaway config and returns the string.
fn render(expr: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let init = format!(
        "return {{ options = {{ out = archix.option {{ type = archix.types.str }} }}, config = {{ out = {expr} }} }}"
    );
    fs::write(dir.path().join("init.lua"), init).unwrap();
    let evaluation = evaluate(&Input {
        config_dir: dir.path().to_path_buf(),
        hostname: "x".into(),
        ..Default::default()
    })
    .unwrap();
    match &evaluation.config {
        Value::Map(map) => match &map["out"] {
            Value::Str(s) => s.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn yaml_quotes_strings_and_nests() {
    assert_eq!(
        render(r#"archix.formats.yaml { name = "a: b", list = { 1, "two" }, nested = { on = true }, empty = {} }"#),
        "\"empty\": {}\n\"list\":\n  - 1\n  - \"two\"\n\"name\": \"a: b\"\n\"nested\":\n  \"on\": true\n"
    );
}

#[test]
fn kconfig_nests_groups() {
    assert_eq!(
        render(r#"archix.formats.kconfig { General = { ColorScheme = "Breeze", Window = { Maximized = true } } }"#),
        "[General]\nColorScheme=Breeze\n\n[General][Window]\nMaximized=true\n"
    );
}

#[test]
fn kdl_nodes_and_arguments() {
    assert_eq!(
        render(
            r#"archix.formats.kdl { layout = { gaps = 16, ["focus-ring"] = { width = 2.5 } }, spawn = { "waybar", "mako" } }"#
        ),
        "layout {\n    focus-ring {\n        width 2.5\n    }\n    gaps 16\n}\nspawn \"waybar\" \"mako\"\n"
    );
}

#[test]
fn key_value_and_ini_with_joined_lists() {
    assert_eq!(
        render(r#"archix.formats.key_value({ b = 2, a = "x" }, { separator = " = " })"#),
        "a = x\nb = 2\n"
    );
    assert_eq!(
        render(r#"archix.formats.ini({ volume = 50, Section = { list = { "a", "b" } } }, { list_separator = ";" })"#),
        "volume=50\n\n[Section]\nlist=a;b\n"
    );
}

#[test]
fn program_builds_a_thin_module() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("init.lua"),
        r#"return {
          imports = { archix.program "foo" { file = ".config/foo/foo.toml", format = "toml" } },
          config = { home = { user = "me" }, programs = { foo = { enable = true, settings = { a = 1 } } } },
        }"#,
    )
    .unwrap();
    let evaluation = evaluate(&Input {
        config_dir: dir.path().to_path_buf(),
        hostname: "x".into(),
        ..Default::default()
    })
    .unwrap();
    let json = serde_json::to_value(&evaluation.config).unwrap();
    assert_eq!(json["home"]["files"][".config/foo/foo.toml"]["text"], "a = 1\n");
    assert!(json["packages"].as_array().unwrap().iter().any(|p| p == "foo"));
}
