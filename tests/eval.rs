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
fn option_descriptions_say_what_waits_for_the_next_boot() {
    let dir = config(&[("init.lua", "return {}")]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_archix"))
        .args([
            "--config",
            dir.path().to_str().unwrap(),
            "--host",
            "desktop",
            "options",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let options: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    let description = |path: &str| -> String {
        options
            .iter()
            .find(|option| option["path"] == path)
            .and_then(|option| option["description"].as_str())
            .unwrap_or_else(|| panic!("no description for {path}"))
            .to_string()
    };
    for stale in [
        "the running one",
        "at the end of every switch",
        "changed this file, before the hooks",
    ] {
        let found: Vec<&str> = options
            .iter()
            .filter(|option| option["description"].as_str().is_some_and(|text| text.contains(stale)))
            .filter_map(|option| option["path"].as_str())
            .collect();
        assert!(found.is_empty(), "{found:?} still say {stale:?}");
    }
    for (path, says) in [
        ("hooks", "first boot"),
        ("hooks", "before networking"),
        ("hooks.<name>.user", "no session bus"),
        ("files", "next boot"),
        ("files.<name>.on_change", "first boot"),
        ("files.<name>.on_change", "right after it's written"),
        ("home.files.<name>.on_change", "right after"),
        ("home.files", "without waiting for the boot"),
        ("services", "next boot"),
        ("user_services", "next boot"),
        ("firewall.enable", "next boot"),
        ("file_systems", "next boot"),
        ("file_systems", "rollback brings back"),
        ("swap", "next boot"),
        ("zram.enable", "next boot"),
        ("flatpak.apps", "first boot"),
    ] {
        let text = description(path);
        assert!(text.contains(says), "{path}: {text}");
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

/// A config that puts `expr` into a declared option, so it evaluates cleanly unless `expr` itself fails.
fn probe(expr: &str) -> String {
    format!(
        "return {{\n  options = {{ probe = archix.option {{ type = archix.types.str }} }},\n  config = {{ probe = tostring({expr}) }},\n}}"
    )
}

/// The error of evaluating `expr` in a config that also holds `files`, or what it evaluated to.
fn escape_err(expr: &str, files: &[(&str, &str)]) -> String {
    let init = probe(expr);
    let dir = config(&[&[("init.lua", init.as_str())], files].concat());
    error_at(dir.path())
}

fn error_at(dir: &Path) -> String {
    match eval_at(dir, "desktop") {
        Ok(evaluation) => format!("probe = {}", serde_json::to_value(&evaluation.config).unwrap()["probe"]),
        Err(e) => e.to_string(),
    }
}

/// Fails listing every case whose error lacks what it should say, so a weakened sandbox shows each escape at once.
fn assert_refused(cases: &[(String, String, String)]) {
    let escaped: Vec<String> = cases
        .iter()
        .filter(|(_, says, err)| !err.contains(says.as_str()))
        .map(|(case, says, err)| format!("{case}: wanted {says:?}, got {err}"))
        .collect();
    assert!(escaped.is_empty(), "{}", escaped.join("\n"));
}

#[test]
fn the_probe_config_evaluates_when_the_expression_is_allowed() {
    assert_eq!(
        eval_ok(&[("init.lua", &probe("string.upper(\"ok\")"))])["probe"],
        json!("OK")
    );
}

#[test]
fn sandbox_has_no_environment_clock_or_files() {
    let cases: Vec<_> = [
        ("os.getenv(\"HOME\")", "os"),
        ("os.time()", "os"),
        ("os.clock()", "os"),
        ("os.date()", "os"),
        ("os.execute(\"true\")", "os"),
        ("io.open(\"init.lua\")", "io"),
        ("io.popen(\"true\")", "io"),
        ("io.lines(\"/etc/hostname\")", "io"),
        ("debug.getinfo(1)", "debug"),
        ("debug.getregistry()", "debug"),
        ("package.loadlib(\"libc.so.6\", \"getenv\")", "package"),
    ]
    .into_iter()
    .map(|(expr, global)| {
        (
            expr.to_string(),
            format!("init.lua:3: attempt to index a nil value (global '{global}')"),
            escape_err(expr, &[]),
        )
    })
    .collect();
    assert_refused(&cases);
}

#[test]
fn sandbox_has_no_loaders_randomness_or_collector() {
    let cases: Vec<_> = [
        ("load(\"return 1\")", "global 'load'"),
        ("loadfile(\"init.lua\")", "global 'loadfile'"),
        ("dofile(\"init.lua\")", "global 'dofile'"),
        ("collectgarbage(\"count\")", "global 'collectgarbage'"),
        ("math.random()", "field 'random'"),
        ("math.randomseed(1)", "field 'randomseed'"),
        ("string.dump(tostring)", "field 'dump'"),
        ("(\"\").dump(tostring)", "field 'dump'"),
    ]
    .into_iter()
    .map(|(expr, missing)| {
        (
            expr.to_string(),
            format!("init.lua:3: attempt to call a nil value ({missing})"),
            escape_err(expr, &[]),
        )
    })
    .collect();
    assert_refused(&cases);
}

#[test]
fn require_loads_only_text_modules_inside_the_config_dir() {
    let mut cases = vec![(
        "a C module or a search path outside the config".to_string(),
        "module \"socket\" not found (looked for lua/socket.lua and lua/socket/init.lua)".to_string(),
        escape_err("require(\"socket\")", &[]),
    )];
    for name in ["../../etc/passwd", "/etc/passwd", "a/b", "x.."] {
        cases.push((
            name.to_string(),
            format!("invalid module name {name:?}; use dotted names like \"hosts.desktop\""),
            escape_err(&format!("require({name:?})"), &[]),
        ));
    }
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("leak.lua"), "return \"outside\"").unwrap();
    let dir = config(&[("init.lua", &probe("require(\"leak\")"))]);
    fs::create_dir_all(dir.path().join("lua")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("leak.lua"), dir.path().join("lua/leak.lua")).unwrap();
    cases.push((
        "a module linked from outside the config".to_string(),
        "module \"leak\": lua/leak.lua leaves the config directory".to_string(),
        error_at(dir.path()),
    ));
    cases.push((
        "a precompiled chunk".to_string(),
        "attempt to load a binary chunk (mode is 't')".to_string(),
        escape_err("require(\"bin\")", &[("lua/bin.lua", "\x1bLua\x54\x00")]),
    ));
    assert_refused(&cases);
}

#[test]
fn read_file_stays_inside_the_config_dir() {
    let out = eval_ok(&[
        ("init.lua", &probe("archix.read_file(\"files/motd\")")),
        ("files/motd", "hello"),
    ]);
    assert_eq!(out["probe"], json!("hello"));

    let parent = TempDir::new().unwrap();
    let dir = parent.path().join("config");
    fs::create_dir_all(dir.join("files")).unwrap();
    let secret = parent.path().join("secret");
    fs::write(&secret, "secret").unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("files/link")).unwrap();
    let mut cases = Vec::new();
    for (path, says) in [
        ("../secret", "path leaves the config directory"),
        ("files/link", "path leaves the config directory"),
        (
            secret.to_str().unwrap(),
            "path must be relative to the config directory",
        ),
    ] {
        assert_eq!(fs::read_to_string(&secret).unwrap(), "secret", "the file exists");
        fs::write(dir.join("init.lua"), probe(&format!("archix.read_file({path:?})"))).unwrap();
        cases.push((
            path.to_string(),
            format!("archix.read_file({path:?}): {says}"),
            error_at(&dir),
        ));
    }
    assert_refused(&cases);
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
fn pairs_refuses_keys_with_no_reproducible_order() {
    let err = eval_err(&[(
        "init.lua",
        r#"
        local seen = { [{}] = 1, [{}] = 2 }
        for _ in pairs(seen) do end
        return {}"#,
    )]);
    assert!(err.contains("no reproducible order"), "{err}");
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

#[test]
fn firewall_services_have_built_in_ports_that_modules_override_or_add_to() {
    let out = eval_ok(&[(
        "init.lua",
        r#"return { firewall = { enable = true, allowed_services = { "ssh", "syncthing" } } }"#,
    )]);
    let ports = &out["firewall"]["service_ports"];
    assert_eq!(
        ports["syncthing"],
        json!({
            "tcp": [22000],
            "udp": [21027, 22000],
            "tcp_ranges": [],
            "udp_ranges": [],
            "sources": "lan",
            "source": "https://github.com/NixOS/nixpkgs/blob/nixos-unstable/nixos/modules/services/networking/syncthing.nix",
        })
    );
    assert_eq!(ports["kdeconnect"]["udp_ranges"], json!([{ "from": 1714, "to": 1764 }]));
    for (name, entry) in ports.as_object().unwrap() {
        let source = entry["source"].as_str().unwrap_or_default();
        assert!(
            source.starts_with("https://"),
            "{name} names no upstream source: {entry}"
        );
        assert!(
            entry["sources"] == "lan" || entry["sources"] == "any",
            "{name}: {entry}"
        );
    }
    let open_to_any: Vec<&str> = ports
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, entry)| entry["sources"] == "any")
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        open_to_any,
        [
            "http",
            "https",
            "minecraft",
            "mosh",
            "ssh",
            "steam-dedicated-server",
            "tailscale",
            "transmission",
            "wireguard"
        ],
        "only what is meant to be reached from anywhere is open to any source by default"
    );
    assert_eq!(out["firewall"]["allowed_services"], json!(["ssh", "syncthing"]));
    assert_eq!(out["packages"], json!(["nftables"]));

    let out = eval_ok(&[
        (
            "init.lua",
            r#"return { imports = { "a", "b" }, config = { firewall = { service_ports = { ssh = { tcp = { 2222 } } } } } }"#,
        ),
        (
            "lua/a.lua",
            r#"return { firewall = { allowed_services = { "ssh", "valheim" }, service_ports = { valheim = { udp_ranges = { { from = 2456, to = 2458 } } }, kdeconnect = archix.default { sources = archix.force("any") } } } }"#,
        ),
        (
            "lua/b.lua",
            r#"return { config = function() return { firewall = { allowed_services = { "ssh" }, service_ports = { mdns = archix.force { udp = { 5353, 5354 } } } } } end }"#,
        ),
    ]);
    let ports = &out["firewall"]["service_ports"];
    assert_eq!(
        ports["ssh"]["tcp"],
        json!([2222]),
        "a plain definition replaces the built-in one"
    );
    assert_eq!(ports["mdns"]["udp"], json!([5353, 5354]));
    assert_eq!(ports["valheim"]["udp_ranges"], json!([{ "from": 2456, "to": 2458 }]));
    assert_eq!(
        ports["valheim"]["sources"], "any",
        "a service of your own is open to any source unless it says"
    );
    assert_eq!(ports["ssh"]["sources"], "any");
    assert_eq!(
        (&ports["kdeconnect"]["sources"], &ports["kdeconnect"]["tcp_ranges"]),
        (&json!("any"), &json!([{ "from": 1714, "to": 1764 }])),
        "archix.default keeps a built-in's ports while archix.force changes its sources"
    );
    assert_eq!(ports["https"]["tcp"], json!([443]), "the other built-ins stay");
    let mut services: Vec<&str> = out["firewall"]["allowed_services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    services.sort_unstable();
    assert_eq!(
        services,
        ["ssh", "ssh", "valheim"],
        "lists concatenate; the ruleset opens each service once"
    );
}

#[test]
fn naming_an_unknown_firewall_service_lists_the_known_ones() {
    let err = eval_err(&[
        (
            "init.lua",
            r#"return { imports = { "game" }, config = { firewall = { allowed_services = { "ssh", "stean", "valheim" } } } }"#,
        ),
        (
            "lua/game.lua",
            r#"return { config = function() return { firewall = { service_ports = { valheim = { udp = { 2456 } } } } } end }"#,
        ),
    ]);
    assert!(
        err.contains(r#"names "stean", which firewall.service_ports doesn't define"#),
        "{err}"
    );
    assert!(err.contains("known services: alvr, "), "{err}");
    assert!(
        err.contains(", steam, steam-dedicated-server, ") && err.contains(", valheim, "),
        "{err}"
    );
}

/// The evaluated and rendered system of an init.lua returning `body`, or the error either gives.
fn rendered(body: &str) -> Result<archix::system::Rendered, String> {
    let dir = config(&[("init.lua", &format!("return {{ {body} }}"))]);
    let evaluation = eval_at(dir.path(), "desktop").map_err(|e| e.to_string())?;
    archix::system::render(&evaluation.config, dir.path()).map_err(|e| format!("{e:#}"))
}

fn rendered_ok(body: &str) -> archix::system::Rendered {
    rendered(body).unwrap_or_else(|e| panic!("{body}: {e}"))
}

const HASH: &str = "$y$j9T$u2f4nll1JxfH3hjxKg.8P1$iQmwHBe.iDFgnhAJVdpekC.aTQ8uIWtwoFNGILtAHV9";
const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl me@laptop";

#[test]
fn credentials_render_into_their_files_and_bring_their_packages() {
    let body = format!(
        r#"users = {{ alice = {{ groups = {{ "wheel" }}, hashed_password = "{HASH}", ssh_authorized_keys = {{ "{KEY}" }} }} }},
        sudo = {{ enable = true, extra_rules = {{ "alice ALL=(ALL) NOPASSWD: /usr/bin/systemctl" }} }},
        tmpfiles = {{ "d /var/lib/foo 0750 alice alice -" }}"#
    );
    let dir = config(&[("init.lua", &format!("return {{ {body} }}"))]);
    let config = eval_at(dir.path(), "desktop").unwrap().config;
    let packages = serde_json::to_value(&config).unwrap()["packages"].clone();
    let has = |name: &str| packages.as_array().unwrap().iter().any(|p| p == name);
    assert!(has("sudo") && has("openssh"), "{packages}");
    let rendered = archix::system::render(&config, dir.path()).unwrap();
    let files = &rendered.state.files;
    let text = |path: &str| String::from_utf8(rendered.blobs[&files[path].sha256].clone()).unwrap();
    assert_eq!(text("/etc/ssh/authorized_keys.d/alice"), format!("{KEY}\n"));
    assert_eq!(files["/etc/ssh/authorized_keys.d/alice"].mode, 0o644);
    assert!(
        text("/etc/ssh/sshd_config.d/10-archix-authorized-keys.conf")
            .contains("AuthorizedKeysFile .ssh/authorized_keys /etc/ssh/authorized_keys.d/%u\n")
    );
    assert_eq!(files["/etc/sudoers.d/archix"].mode, 0o440);
    assert!(
        text("/etc/sudoers.d/archix")
            .contains("%wheel ALL=(ALL:ALL) ALL\nalice ALL=(ALL) NOPASSWD: /usr/bin/systemctl\n")
    );
    assert_eq!(
        text("/etc/tmpfiles.d/archix.conf"),
        "d /var/lib/foo 0750 alice alice -\n"
    );
    assert_eq!(rendered.state.users["alice"].hashed_password.as_deref(), Some(HASH));
    let none = rendered_ok("users = { bob = {} }");
    assert!(
        !none
            .state
            .files
            .keys()
            .any(|path| path.contains("ssh") || path.contains("sudoers"))
    );
}

#[test]
fn credentials_that_arent_what_they_should_be_are_refused_without_printing_the_hash() {
    for (body, says) in [
        (
            r#"users = { alice = { hashed_password = "hunter2" } }"#.to_string(),
            "looks like a plaintext password",
        ),
        (
            r#"users = { alice = { initial_hashed_password = "$1$x$abcdefghijklmnopqrstuv" } }"#.to_string(),
            "old hash type",
        ),
        (
            format!(r#"users = {{ alice = {{ hashed_password = "{}" }} }}"#, &HASH[..30]),
            "isn't a whole one",
        ),
        (
            format!(r#"users = {{ alice = {{ hashed_password = "{HASH}", initial_hashed_password = "{HASH}" }} }}"#),
            "not both",
        ),
        (
            r#"users = { alice = { ssh_authorized_keys = { "AAAAC3Nza me@laptop" } } }"#.to_string(),
            "ssh_authorized_keys[1] names no key type",
        ),
        (
            r#"users = { alice = { ssh_authorized_keys = { "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl" } } }"#.to_string(),
            "isn't a ssh-rsa key",
        ),
        (r#"tmpfiles = { "R /var/lib" }"#.to_string(), "would reach into /var/lib/archix"),
        (r#"tmpfiles = { "d /archix/x 0755 root root -" }"#.to_string(), "touches /archix"),
        (
            r#"boot = { esp = "/boot/efi" }, tmpfiles = { "r /boot/efi/x" }"#.to_string(),
            "touches /boot/efi",
        ),
        (r#"tmpfiles = { "y /x" }"#.to_string(), "isn't a tmpfiles.d type"),
        (
            r#"sudo = { enable = true, extra_rules = { "a\nb" } }"#.to_string(),
            "one sudoers line",
        ),
    ] {
        let error = rendered(&body).err().unwrap_or_else(|| panic!("{body} was accepted"));
        assert!(error.contains(says), "{body}: {error}");
        assert!(!error.contains(&HASH[10..]), "{error}");
    }
    let warned = rendered_ok(r#"tmpfiles = { "R /home/*/.cache/thumbnails" }"#).warnings;
    assert!(warned.iter().any(|w| w.contains("under /home")), "{warned:?}");
}

#[test]
fn file_systems_swap_and_zram_evaluate_and_render_into_fstab_lines() {
    let dir = config(&[(
        "init.lua",
        r#"return {
  file_systems = {
    ["/mnt/data"] = { device = "UUID=1234", fs_type = "ext4", check = true },
    ["/srv/www"] = { device = "/var/www", options = { "bind" } },
  },
  swap = { { file = "/swap/swapfile", size = "1G" } },
  zram = { enable = true, size = "ram / 2" },
}"#,
    )]);
    let evaluation = eval_at(dir.path(), "desktop").unwrap();
    let out = serde_json::to_value(&evaluation.config).unwrap();
    assert_eq!(
        out["file_systems"]["/mnt/data"],
        json!({ "device": "UUID=1234", "fs_type": "ext4", "options": [], "check": true, "required": null, "encrypted": null })
    );
    assert_eq!(out["packages"], json!(["zram-generator"]), "zram brings zram-generator");
    let rendered = archix::system::render(&evaluation.config, dir.path()).unwrap();
    let lines: Vec<String> = rendered.state.fstab.iter().map(|line| line.line()).collect();
    assert_eq!(
        lines,
        [
            "UUID=1234 /mnt/data ext4 nofail,x-systemd.device-timeout=30s 0 2",
            "/var/www /srv/www none bind 0 0",
            "/swap/swapfile none swap nofail 0 0",
        ]
    );
    for path in [
        "/etc/systemd/zram-generator.conf",
        "/etc/systemd/system/archix-swapfile-swap-swapfile.service",
    ] {
        assert!(rendered.state.files.contains_key(path), "{path}");
    }
}

#[test]
fn a_config_that_writes_fstab_or_what_the_mount_options_write_is_refused() {
    for (body, says) in [
        (
            r#"return { files = { ["/etc/fstab"] = { text = "UUID=1 / ext4 rw 0 1" } } }"#,
            "declare mounts in file_systems and swap",
        ),
        (
            r#"return { zram = { enable = true }, files = { ["/etc/systemd/zram-generator.conf"] = { text = "[zram0]" } } }"#,
            "written by archix from zram",
        ),
        (
            r#"return { files = { ["/etc/crypttab"] = { text = "x" } }, file_systems = { ["/mnt/s"] = { device = "/dev/mapper/s", encrypted = { device = "UUID=1" } } } }"#,
            "written by archix from file_systems' encrypted devices",
        ),
        (
            r#"return { file_systems = { ["/var/lib/pacman"] = { device = "UUID=1" } } }"#,
            "pacman database",
        ),
        (
            r#"return { file_systems = { ["/mnt/x"] = { fs_type = "ext4" } } }"#,
            "device must name what to mount",
        ),
        (
            r#"return { swap = { { file = "/archix/swapfile", size = "1G" } } }"#,
            "deployments live in /archix",
        ),
    ] {
        let dir = config(&[("init.lua", body)]);
        let evaluation = eval_at(dir.path(), "desktop").unwrap();
        let error = archix::system::render(&evaluation.config, dir.path())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains(says), "{body}: {error}");
    }
}
