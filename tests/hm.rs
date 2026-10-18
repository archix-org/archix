//! Cross-validates archix's ports of Home Manager modules against Home Manager's own test outputs (tests/hm).

use std::fs;
use std::path::Path;

use archix::eval::{Input, evaluate};
use archix::system::render;
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "case")]
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    module: String,
    input: String,
    file: String,
    expected: String,
    #[serde(default)]
    replace: Vec<(String, String)>,
    #[serde(default)]
    from_line: Option<String>,
    #[serde(default)]
    also_exists: Vec<String>,
}

const HOME: &str = "/home/hm-user";

fn from(text: &str, marker: &Option<String>) -> String {
    match marker {
        Some(marker) => match text.find(marker.as_str()) {
            Some(i) => text[i..].to_string(),
            None => panic!("{marker:?} not found in:\n{text}"),
        },
        None => text.to_string(),
    }
}

#[test]
fn ports_render_what_home_manager_renders() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/hm");
    let manifest: Manifest = toml::from_str(&fs::read_to_string(root.join("cases.toml")).unwrap()).unwrap();
    let mut failures = Vec::new();
    for case in &manifest.cases {
        let dir = tempfile::tempdir().unwrap();
        fs::copy(root.join(&case.input), dir.path().join("init.lua")).unwrap();
        let evaluation = match evaluate(&Input {
            config_dir: dir.path().to_path_buf(),
            hostname: "hm".into(),
            ..Default::default()
        }) {
            Ok(evaluation) => evaluation,
            Err(e) => {
                failures.push(format!("{}: evaluation failed:\n{e}", case.input));
                continue;
            }
        };
        let rendered = render(&evaluation.config, dir.path()).unwrap();
        let path = format!("{HOME}/{}", case.file);
        let Some(file) = rendered.state.files.get(&path) else {
            failures.push(format!("{}: {} was not rendered", case.input, case.file));
            continue;
        };
        let actual = String::from_utf8(rendered.blobs[&file.sha256].clone()).unwrap();
        let mut expected = fs::read_to_string(root.join(&case.expected))
            .unwrap()
            .replace("Home Manager", "archix");
        for (from, to) in &case.replace {
            expected = expected.replace(from, to);
        }
        let (actual, expected) = (from(&actual, &case.from_line), from(&expected, &case.from_line));
        if actual != expected {
            failures.push(format!(
                "{} ({}): {} differs\n--- expected (Home Manager)\n{expected}\n--- actual (archix)\n{actual}",
                case.input, case.module, case.file
            ));
        }
        for extra in &case.also_exists {
            if !rendered.state.files.contains_key(&format!("{HOME}/{extra}")) {
                failures.push(format!("{}: {extra} was not rendered", case.input));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n\n{}",
        failures.len(),
        manifest.cases.len(),
        failures.join("\n\n")
    );
}
