//! Lua→home-manager (SCOPE 7): the round trip Lua → Nix → value, hm.path copies, refusals and the generated flake dir.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use archix::eval::{Input, evaluate};
use archix::home::hm::generate::{Entry, Secrets, generate_into, render};
use archix::home::hm::{self, Account, HmLock, Refusal, System};
use archix::value::Value;
use tempfile::TempDir;

fn lock() -> HmLock {
    HmLock {
        home_manager: "0123456789abcdef0123456789abcdef01234567".into(),
        nixpkgs: "89abcdef0123456789abcdef0123456789abcdef".into(),
    }
}

fn config_dir(init: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("init.lua"), init).unwrap();
    dir
}

fn eval(dir: &Path) -> Result<Value, String> {
    evaluate(&Input {
        config_dir: dir.to_path_buf(),
        hostname: "box".into(),
        ..Default::default()
    })
    .map(|e| e.config)
    .map_err(|e| e.to_string())
}

fn home_nix(dir: &Path) -> anyhow::Result<String> {
    let config = eval(dir).map_err(anyhow::Error::msg)?;
    let rendered = render("al", Path::new("/home/al"), &config, dir, &lock(), &Secrets::default())?;
    Ok(rendered.home_nix)
}

// A generated value: what Lua writes and what the Nix it becomes must evaluate to.
#[derive(Debug, Clone)]
enum G {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<G>),
    Map(BTreeMap<String, G>),
    EmptyList,
    EmptyAttrs,
}

fn lua_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(c),
            c => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
        }
    }
    out.push('"');
    out
}

impl G {
    fn lua(&self) -> String {
        match self {
            G::Null => "hm.null".into(),
            G::Bool(b) => b.to_string(),
            G::Int(i64::MIN) => "math.mininteger".into(),
            G::Int(i) => i.to_string(),
            G::Float(f) => format!("{f:?}"),
            G::Str(s) => lua_string(s),
            G::List(items) => format!("{{ {} }}", items.iter().map(G::lua).collect::<Vec<_>>().join(", ")),
            G::Map(map) => {
                let fields: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("[{}] = {}", lua_string(k), v.lua()))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
            G::EmptyList => "hm.list{}".into(),
            G::EmptyAttrs => "hm.attrs{}".into(),
        }
    }

    fn value(&self) -> Value {
        match self {
            G::Null => Value::Null,
            G::Bool(b) => Value::Bool(*b),
            G::Int(i) => Value::Int(*i),
            G::Float(f) => Value::Float(*f),
            G::Str(s) => Value::Str(s.clone()),
            G::List(items) => Value::List(items.iter().map(G::value).collect()),
            G::Map(map) => Value::Map(map.iter().map(|(k, v)| (k.clone(), v.value())).collect()),
            G::EmptyList => Value::List(Vec::new()),
            G::EmptyAttrs => Value::Map(BTreeMap::new()),
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn string(&mut self) -> String {
        const PIECES: [&str; 24] = [
            "${", "${x}", "$", "$${", "\\", "\\${", "\"", "''", "'''", "''${", "\n", "\t", "\r", "\u{1}", "\u{7f}",
            "é", "✓", "😀", "\u{85}", "\u{2028}", "a", " ", "{", "}",
        ];
        (0..self.below(8)).map(|_| PIECES[self.below(PIECES.len())]).collect()
    }

    fn key(&mut self) -> String {
        const KEYS: [&str; 14] = [
            "enable", "a-b", "_x", "x'", "1st", "with", "or", "in", "a.b", "a b", "", "${k}", "é", "__curPos",
        ];
        match self.below(3) {
            0 => self.string(),
            _ => KEYS[self.below(KEYS.len())].to_string(),
        }
    }

    fn float(&mut self) -> f64 {
        const FLOATS: [f64; 10] = [1.0, -0.5, 0.1, 1e-7, 1.5e300, 123456.0, -2.25, 5e-324, 1e16, 3.0e-5];
        match self.below(2) {
            0 => FLOATS[self.below(FLOATS.len())],
            _ => (self.next() % 1_000_000) as f64 / 64.0 - 5000.0,
        }
    }

    fn value_at(&mut self, depth: usize) -> G {
        let kinds = if depth >= 3 { 7 } else { 9 };
        match self.below(kinds) {
            0 => G::Null,
            1 => G::Bool(self.below(2) == 0),
            2 => G::Int(match self.below(4) {
                0 => i64::MIN,
                1 => i64::MAX,
                _ => self.next() as i64 % 100_000,
            }),
            3 => G::Float(self.float()),
            4 | 5 => G::Str(self.string()),
            6 => [G::EmptyList, G::EmptyAttrs][self.below(2)].clone(),
            7 => G::List((0..1 + self.below(3)).map(|_| self.value_at(depth + 1)).collect()),
            _ => self.map(depth + 1),
        }
    }

    fn map(&mut self, depth: usize) -> G {
        let mut map = BTreeMap::new();
        while map.len() < 1 + self.below(4) {
            let key = self.key();
            map.insert(key, self.value_at(depth));
        }
        G::Map(map)
    }
}

/// A parser for the Nix subset home.nix uses, by Nix's own lexing rules for strings.
struct Nix<'a> {
    s: &'a [u8],
    at: usize,
}

impl<'a> Nix<'a> {
    fn ws(&mut self) {
        loop {
            while self.at < self.s.len() && self.s[self.at].is_ascii_whitespace() {
                self.at += 1;
            }
            if self.s.get(self.at) == Some(&b'#') {
                while self.at < self.s.len() && self.s[self.at] != b'\n' {
                    self.at += 1;
                }
                continue;
            }
            return;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        self.ws();
        if self.s[self.at..].starts_with(token.as_bytes()) {
            self.at += token.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &str) {
        assert!(
            self.eat(token),
            "expected {token:?} at {:?}",
            String::from_utf8_lossy(&self.s[self.at..])
        );
    }

    fn string(&mut self) -> String {
        self.expect("\"");
        let mut out = Vec::new();
        loop {
            match self.s[self.at] {
                b'"' => {
                    self.at += 1;
                    return String::from_utf8(out).unwrap();
                }
                b'\\' => {
                    out.push(match self.s[self.at + 1] {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        c => c,
                    });
                    self.at += 2;
                }
                b'$' if self.s[self.at + 1] == b'{' => panic!("an interpolation in a generated string"),
                b'$' if self.s[self.at + 1] == b'\\' => {
                    out.push(b'$');
                    self.at += 1;
                }
                c => {
                    out.push(c);
                    self.at += 1;
                }
            }
        }
    }

    fn ident(&mut self) -> String {
        self.ws();
        let start = self.at;
        while self.at < self.s.len() && (self.s[self.at].is_ascii_alphanumeric() || b"_'-".contains(&self.s[self.at])) {
            self.at += 1;
        }
        String::from_utf8(self.s[start..self.at].to_vec()).unwrap()
    }

    fn name(&mut self) -> String {
        self.ws();
        match self.s[self.at] {
            b'"' => self.string(),
            _ => self.ident(),
        }
    }

    fn number(&mut self) -> Value {
        self.ws();
        let start = self.at;
        while self.at < self.s.len() && (self.s[self.at].is_ascii_alphanumeric() || b".+-".contains(&self.s[self.at])) {
            if b"+-".contains(&self.s[self.at]) && !matches!(self.s[self.at - 1], b'e' | b'E') {
                break;
            }
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.at]).unwrap();
        match text.contains(['.', 'e']) {
            true => Value::Float(text.parse().unwrap()),
            false => Value::Int(text.parse().unwrap()),
        }
    }

    fn expr(&mut self) -> Value {
        self.ws();
        if self.eat("(") {
            let value = if self.eat("-") {
                match self.number() {
                    Value::Int(i) if self.eat("-") => {
                        assert_eq!(self.number(), Value::Int(1));
                        Value::Int(-i - 1)
                    }
                    Value::Int(i) => Value::Int(-i),
                    Value::Float(f) => Value::Float(-f),
                    other => panic!("{other:?}"),
                }
            } else if self.eat("./files") {
                self.expect("+");
                Value::Str(format!("./files{}", self.string()))
            } else {
                panic!("unexpected parenthesis");
            };
            self.expect(")");
            return value;
        }
        if self.eat("[") {
            let mut items = Vec::new();
            while !self.eat("]") {
                items.push(self.expr());
            }
            return Value::List(items);
        }
        if self.eat("{") {
            let mut map = BTreeMap::new();
            while !self.eat("}") {
                let key = self.name();
                self.expect("=");
                let value = self.expr();
                self.expect(";");
                assert!(map.insert(key, value).is_none(), "a duplicate attribute");
            }
            return Value::Map(map);
        }
        if self.s[self.at] == b'"' {
            return Value::Str(self.string());
        }
        if self.s[self.at..].starts_with(b"./") {
            let start = self.at;
            while self.at < self.s.len() && !self.s[self.at].is_ascii_whitespace() && self.s[self.at] != b';' {
                self.at += 1;
            }
            return Value::Str(String::from_utf8(self.s[start..self.at].to_vec()).unwrap());
        }
        if self.s[self.at].is_ascii_digit() {
            return self.number();
        }
        match self.ident().as_str() {
            "null" => Value::Null,
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn home_nix(text: &str) -> Value {
        let mut p = Nix {
            s: text.as_bytes(),
            at: 0,
        };
        p.expect("{");
        for arg in ["config", ",", "lib", ",", "pkgs", ",", "...", "}", ":"] {
            p.expect(arg);
        }
        let value = p.expr();
        p.ws();
        assert_eq!(p.at, p.s.len(), "trailing text");
        value
    }
}

fn nix_on_path() -> bool {
    Command::new("sh")
        .args(["-c", "command -v nix"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn from_json(json: serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) if !n.is_f64() => Value::Int(i),
            _ => Value::Float(n.as_f64().unwrap()),
        },
        serde_json::Value::String(s) => Value::Str(s),
        serde_json::Value::Array(items) => Value::List(items.into_iter().map(from_json).collect()),
        serde_json::Value::Object(map) => Value::Map(map.into_iter().map(|(k, v)| (k, from_json(v))).collect()),
    }
}

/// `nix eval --json` of a home.nix called with empty arguments.
fn nix_eval(home_nix: &str) -> Value {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("home.nix"), home_nix).unwrap();
    let expr = format!(
        "import {}/home.nix {{ config = {{}}; lib = {{}}; pkgs = {{}}; }}",
        dir.path().display()
    );
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "eval",
            "--json",
            "--impure",
            "--expr",
            &expr,
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{home_nix}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    from_json(json)
}

fn round_trip_cases(n: u64) -> Vec<(G, String)> {
    (1..=n)
        .map(|seed| {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let g = rng.map(0);
            let dir = config_dir(&format!(
                "return {{ home = {{ al = {{ home_manager = {} }} }} }}\n",
                g.lua()
            ));
            let text = home_nix(dir.path()).unwrap_or_else(|e| panic!("seed {seed}: {e:#}\n{}", g.lua()));
            (g, text)
        })
        .collect()
}

#[test]
fn lua_values_round_trip_through_the_generated_nix() {
    let cases = round_trip_cases(300);
    let all: String = cases.iter().map(|(_, text)| text.as_str()).collect();
    for want in [
        "\\${",
        "''",
        "\\\\",
        "\\\"",
        "\\n",
        "[ ]",
        "{ }",
        "null",
        "(-",
        "e-",
        "\"with\" =",
        "😀",
    ] {
        assert!(all.contains(want), "no case covers {want:?}");
    }
    for (g, text) in cases {
        assert_eq!(Nix::home_nix(&text), g.value(), "{}\n{text}", g.lua());
    }
}

#[test]
#[ignore = "needs nix on PATH; run with --ignored where nix is installed"]
fn lua_values_round_trip_through_nix_eval() {
    assert!(nix_on_path(), "nix isn't on PATH");
    for (g, text) in round_trip_cases(100) {
        assert_eq!(nix_eval(&text), g.value(), "{}\n{text}", g.lua());
    }
}

#[test]
fn nix_float_literals_parse_as_the_same_number() {
    for f in [1.0, 0.1, 1e-7, 1.5e300, 5e-324, 1e16, 123456.0, 2.5e-3] {
        let text = hm::nix::float(f, "p").unwrap();
        assert_eq!(
            Nix {
                s: text.as_bytes(),
                at: 0
            }
            .number(),
            Value::Float(f),
            "{text}"
        );
    }
}

#[test]
fn plain_empty_tables_mixed_tables_nan_and_nul_are_errors() {
    let cases = [
        ("{ x = {} }", "hm.list{}"),
        ("{ x = { 1, a = 2 } }", "mixes"),
        ("{ x = 0/0 }", "NaN"),
        ("{ x = 1/0 }", "Nix float"),
        ("{ x = -1/0 }", "Nix float"),
        ("{ x = \"a\\0b\" }", "NUL"),
        ("{ x = hm.path \"../outside\" }", "leaves the config directory"),
    ];
    for (lua, want) in cases {
        let dir = config_dir(&format!("return {{ home = {{ al = {{ home_manager = {lua} }} }} }}\n"));
        let err = match eval(dir.path()) {
            Err(e) => e,
            Ok(config) => render(
                "al",
                Path::new("/home/al"),
                &config,
                dir.path(),
                &lock(),
                &Secrets::default(),
            )
            .map(|_| String::new())
            .unwrap_or_else(|e| format!("{e:#}")),
        };
        assert!(err.contains(want), "{lua}: {err}");
    }
}

#[test]
fn hm_helpers_render_packages_raw_expressions_and_paths() {
    let dir = config_dir(
        r#"return { home = { al = { home_manager = {
  home = { packages = { hm.pkg "ripgrep", hm.pkg "python3Packages.requests" }, stateVersion = "25.05" },
  programs = { git = { enable = true, extraConfig = hm.raw "lib.mkForce { }" } },
  xdg = { configFile = { ["hypr/hyprland.conf"] = { source = hm.path "hypr/hyprland.conf" } } },
  keep = hm.attrs { a = 1 }, list = hm.list { 1, 2 },
} } } }
"#,
    );
    fs::create_dir(dir.path().join("hypr")).unwrap();
    fs::write(dir.path().join("hypr/hyprland.conf"), "monitor=,preferred,auto,1\n").unwrap();
    let text = home_nix(dir.path()).unwrap();
    for want in [
        "pkgs.ripgrep",
        "pkgs.python3Packages.requests",
        "extraConfig = (\nlib.mkForce { }\n);",
        "source = ./files/hypr/hyprland.conf;",
        "\"hypr/hyprland.conf\" = {",
        "keep = {\n    a = 1;\n  };",
    ] {
        assert!(text.contains(want), "{want:?} not in\n{text}");
    }
}

#[test]
fn a_user_with_both_home_kinds_fails_evaluation_naming_them() {
    let dir = config_dir(
        r#"return { home = { user = "al", al = { home_manager = { programs = { git = { enable = true } } } } } }"#,
    );
    let err = eval(dir.path()).unwrap_err();
    assert!(
        err.contains("home.al.home_manager") && err.contains("user al has both"),
        "{err}"
    );
    let other = config_dir(r#"return { home = { user = "bo", al = { home_manager = { x = 1 }, linger = true } } }"#);
    let config = eval(other.path()).unwrap();
    assert!(hm::linger(&config, "al"));
}

#[test]
fn a_secret_is_refused_on_its_way_into_home_manager() {
    let dir = config_dir(
        r#"return { home = { al = { home_manager = { programs = { gh = { token = "x" .. archix.secret("gh") } } } } } }"#,
    );
    let config = eval(dir.path()).unwrap();
    let err = render(
        "al",
        Path::new("/home/al"),
        &config,
        dir.path(),
        &lock(),
        &Secrets::default(),
    )
    .unwrap_err();
    let refusal = err.downcast_ref::<Refusal>().expect("a refusal, exit 2");
    assert_eq!(
        refusal.0,
        format!("home.al.home_manager.programs.gh.token: {}", hm::SECRET_MESSAGE)
    );
    let placement = archix::secrets::check_placement(&config).unwrap_err().to_string();
    assert!(placement.contains(hm::SECRET_MESSAGE), "{placement}");
}

fn path_config(dir: &Path, rel: &str) {
    fs::write(
        dir.join("init.lua"),
        format!("return {{ home = {{ al = {{ home_manager = {{ src = hm.path {rel:?} }} }} }} }}\n"),
    )
    .unwrap();
}

fn render_paths(dir: &Path, secrets: &Secrets) -> anyhow::Result<Vec<Entry>> {
    let config = eval(dir).map_err(anyhow::Error::msg)?;
    Ok(render("al", Path::new("/home/al"), &config, dir, &lock(), secrets)?.files)
}

#[test]
fn hm_path_copies_files_and_directories_keeping_the_exec_bit() {
    let dir = TempDir::new().unwrap();
    let scripts = dir.path().join("bin");
    fs::create_dir_all(scripts.join("sub")).unwrap();
    fs::write(scripts.join("run"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(scripts.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(scripts.join("sub/data"), "d").unwrap();
    symlink("../run", scripts.join("sub/inside")).unwrap();
    path_config(dir.path(), "./bin/");
    let entries = render_paths(dir.path(), &Secrets::default()).unwrap();
    let file = |rel: &str, data: &str, executable: bool| Entry::File {
        rel: rel.into(),
        data: data.as_bytes().to_vec(),
        executable,
    };
    assert_eq!(
        entries,
        [
            Entry::Dir("bin".into()),
            file("bin/run", "#!/bin/sh\n", true),
            Entry::Dir("bin/sub".into()),
            file("bin/sub/data", "d", false),
            file("bin/sub/inside", "#!/bin/sh\n", true),
        ]
    );

    path_config(dir.path(), "bin/sub/data");
    let entries = render_paths(dir.path(), &Secrets::default()).unwrap();
    assert_eq!(
        entries,
        [
            Entry::Dir("bin".into()),
            Entry::Dir("bin/sub".into()),
            file("bin/sub/data", "d", false)
        ]
    );
}

#[test]
fn hm_path_refuses_escapes_outward_symlinks_and_secret_files() {
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("passwd"), "x").unwrap();
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("conf")).unwrap();
    symlink(outside.path().join("passwd"), dir.path().join("conf/link")).unwrap();
    symlink(outside.path(), dir.path().join("out")).unwrap();

    path_config(dir.path(), "conf");
    let err = format!("{:#}", render_paths(dir.path(), &Secrets::default()).unwrap_err());
    assert!(
        err.contains("conf/link is a symlink to") && err.contains("outside the config directory"),
        "{err}"
    );
    path_config(dir.path(), "out/passwd");
    let err = format!("{:#}", render_paths(dir.path(), &Secrets::default()).unwrap_err());
    assert!(err.contains("leaves the config directory"), "{err}");
    path_config(dir.path(), "../x");
    let err = format!("{:#}", render_paths(dir.path(), &Secrets::default()).unwrap_err());
    assert!(err.contains("leaves the config directory"), "{err}");

    fs::remove_file(dir.path().join("conf/link")).unwrap();
    fs::write(dir.path().join("conf/a.age"), "age-encryption.org/v1\n-> X25519 x\n").unwrap();
    path_config(dir.path(), "conf");
    let err = render_paths(dir.path(), &Secrets::default()).unwrap_err();
    assert!(
        err.downcast_ref::<Refusal>()
            .is_some_and(|r| r.0.contains("age-encrypted")),
        "{err:#}"
    );

    fs::write(dir.path().join("conf/a.age"), "token={{archix-secret:gh}}\n").unwrap();
    let err = render_paths(dir.path(), &Secrets::default()).unwrap_err();
    assert!(
        err.downcast_ref::<Refusal>()
            .is_some_and(|r| r.0.contains("placeholder")),
        "{err:#}"
    );

    fs::write(dir.path().join("conf/a.age"), "token=hunter2-hunter2\n").unwrap();
    let value = archix::secrets::memory::SecretBuf::from_slice(b"hunter2-hunter2").unwrap();
    let values = archix::secrets::live::Values::new(vec![("gh".into(), value)]);
    let secrets = Secrets {
        values: Some(&values),
        ..Default::default()
    };
    let err = render_paths(dir.path(), &secrets).unwrap_err();
    let refusal = err.downcast_ref::<Refusal>().expect("a refusal");
    assert!(
        refusal
            .0
            .starts_with("home.al.home_manager.src: hm.path copies conf/a.age"),
        "{}",
        refusal.0
    );
    assert!(
        refusal.0.contains("secret gh") && !refusal.0.contains("hunter2"),
        "{}",
        refusal.0
    );

    fs::write(dir.path().join("conf/a.age"), "plain").unwrap();
    let declared = Secrets {
        files: [dir.path().join("conf/a.age").canonicalize().unwrap()].into(),
        values: None,
    };
    let err = render_paths(dir.path(), &declared).unwrap_err();
    assert!(
        err.downcast_ref::<Refusal>()
            .is_some_and(|r| r.0.contains("declared secret's file")),
        "{err:#}"
    );
}

fn me() -> (String, Account) {
    let uid = unsafe { libc::geteuid() };
    let home = TempDir::new().unwrap().keep();
    let name = std::env::var("USER").unwrap_or_else(|_| "me".into());
    (
        name,
        Account {
            uid,
            gid: unsafe { libc::getegid() },
            home,
        },
    )
}

#[test]
fn generate_writes_the_flake_dir_as_its_user_and_keeps_the_lock_and_gcroot() {
    use std::os::unix::fs::MetadataExt;
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("bin")).unwrap();
    fs::write(dir.path().join("bin/run"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(dir.path().join("bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
    let (user, account) = me();
    let write = |extra: &str| {
        fs::write(
            dir.path().join("init.lua"),
            format!(
                "return {{ home = {{ [{user:?}] = {{ home_manager = {{ home = {{ stateVersion = \"25.05\" }}, bin = hm.path \"bin\"{extra} }} }} }} }}\n"
            ),
        )
        .unwrap();
        let config = eval(dir.path()).unwrap();
        let out = hm::flake_dir(&account.home, &user);
        let uid = account.uid;
        let got = generate_into(&user, &config, dir.path(), &lock(), &account, &out, uid, None, &System).unwrap();
        assert_eq!(got, out);
        out
    };
    let out = write("");
    assert_eq!(out, account.home.join(".config/archix/home-manager").join(&user));
    let flake = fs::read_to_string(out.join("flake.nix")).unwrap();
    assert!(
        flake.contains("github:NixOS/nixpkgs/89abcdef0123456789abcdef0123456789abcdef"),
        "{flake}"
    );
    assert!(
        flake.contains("github:nix-community/home-manager/0123456789abcdef0123456789abcdef01234567"),
        "{flake}"
    );
    assert!(flake.contains("home.homeDirectory = nixpkgs.lib.mkDefault"), "{flake}");
    assert!(
        fs::read_to_string(out.join("home.nix"))
            .unwrap()
            .contains("bin = ./files/bin;")
    );
    let run = out.join("files/bin/run");
    assert_eq!(fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o755);
    for path in [out.clone(), out.join("home.nix"), out.join("files"), run] {
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().uid(),
            account.uid,
            "{}",
            path.display()
        );
    }

    fs::write(out.join("flake.lock"), "{}").unwrap();
    symlink("/nix/store/x", out.join("gcroot")).unwrap();
    fs::write(out.join("files/stale"), "x").unwrap();
    write(", more = 1");
    assert!(out.join("flake.lock").exists(), "an unchanged flake.nix keeps its lock");
    assert!(out.join("gcroot").symlink_metadata().is_ok());
    assert!(!out.join("files/stale").exists(), "files/ is replaced whole");
    let leftovers: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".archix"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let config = eval(dir.path()).unwrap();
    let moved = HmLock {
        nixpkgs: "f".repeat(40),
        ..lock()
    };
    generate_into(
        &user,
        &config,
        dir.path(),
        &moved,
        &account,
        &out,
        account.uid,
        None,
        &System,
    )
    .unwrap();
    assert!(!out.join("flake.lock").exists(), "new revs drop the old lock");
    fs::remove_dir_all(&account.home).unwrap();
}

#[test]
fn generate_as_root_writes_through_runuser() {
    let account = Account {
        uid: 1000,
        gid: 1000,
        home: "/home/al".into(),
    };
    let dir = PathBuf::from("/home/al/.config/archix/home-manager/al");
    let cmd = hm::generate::write_cmd("al", &account, 0, &dir, Vec::new()).unwrap();
    assert_eq!(cmd.argv[..5], ["runuser", "-u", "al", "--", "sh"]);
    assert_eq!(cmd.argv.last().unwrap(), "/home/al/.config/archix/home-manager/al");
    assert!(
        hm::generate::write_cmd("al", &account, 1001, &dir, Vec::new()).is_err(),
        "another user can't write it"
    );
}

#[test]
#[ignore = "needs a VM with nix and home-manager installed, root, and a user named by ARCHIX_HM_USER; a later job runs it there"]
fn hm_build_and_activate_in_vm() {
    let user = std::env::var("ARCHIX_HM_USER").expect("ARCHIX_HM_USER");
    let dir = config_dir(&format!(
        "return {{ home = {{ [{user:?}] = {{ linger = true, home_manager = {{ home = {{ stateVersion = \"25.05\" }}, programs = {{ git = {{ enable = true, userName = \"archix test\" }} }} }} }} }} }}\n"
    ));
    let config = eval(dir.path()).unwrap();
    let revs = hm::update::resolve(&hm::update::http_get).unwrap();
    let flake = hm::generate(&user, &config, dir.path(), &revs).unwrap();
    let runner = hm::Runner::new(&config);
    let store = runner.build(&user, &flake).unwrap();
    assert!(store.starts_with("/nix/store/"));
    runner.activate(&user, &store).unwrap();
    let home = hm::account(Path::new("/"), &user).unwrap().home;
    let git = fs::read_to_string(home.join(".config/git/config")).unwrap();
    assert!(git.contains("archix test"), "{git}");
    let current = home.join(".local/state/home-manager/gcroots/current-home");
    assert_eq!(current.canonicalize().unwrap(), store);
}
