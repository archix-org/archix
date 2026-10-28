//! Declared secrets end to end: switches, the boot's decrypt step into a ramfs, rollbacks, rotation, eject and the editing commands, each scenario ending with a leak sweep for fresh markers; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use archix::system::password;
use common::*;

/// A machine whose config declares secrets, encrypted to its identity and an admin key, with every command's output kept for the sweep.
struct Lab {
    setup: Setup,
    admin: tempfile::TempDir,
    outputs: RefCell<Vec<String>>,
    markers: RefCell<Vec<(String, Vec<u8>)>>,
}

impl Lab {
    fn new(extra: &[&str]) -> Lab {
        let setup = Setup::new(extra, "");
        let admin = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();
        use age::secrecy::ExposeSecret;
        fs::write(
            admin.path().join("identity"),
            format!("{}\n", identity.to_string().expose_secret()),
        )
        .unwrap();
        fs::write(admin.path().join("recipient"), identity.to_public().to_string()).unwrap();
        let lab = Lab {
            setup,
            admin,
            outputs: RefCell::new(Vec::new()),
            markers: RefCell::new(Vec::new()),
        };
        lab.setup.lay_out();
        lab
    }

    fn admin_recipient(&self) -> String {
        fs::read_to_string(self.admin.path().join("recipient")).unwrap()
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_archix"));
        command
            .args(["--config", self.setup.config.to_str().unwrap(), "--host", "box"])
            .args(args)
            .envs(self.setup.machine.env())
            .env("ARCHIX_AGE_IDENTITY", self.admin.path().join("identity"))
            .envs(env.iter().copied());
        command
    }

    fn keep(&self, output: &Output) -> String {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.outputs.borrow_mut().push(text.clone());
        text
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> (bool, String) {
        let mut command = self.command(args, env);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(bytes) = stdin {
            input.write_all(bytes).unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        (output.status.success(), self.keep(&output))
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, text) = self.run(args, &[], None);
        assert!(ok, "archix {args:?} failed:\n{text}");
        text
    }

    fn fails(&self, args: &[&str]) -> String {
        let (ok, text) = self.run(args, &[], None);
        assert!(!ok, "archix {args:?} should have failed:\n{text}");
        text
    }

    /// Encrypts `value` as secret `name` with `archix secrets set`, from stdin.
    fn set(&self, name: &str, value: &[u8]) {
        let (ok, text) = self.run(&["secrets", "set", name], &[], Some(value));
        assert!(ok, "secrets set {name}:\n{text}");
    }

    /// A fresh marker this scenario's sweep looks for.
    fn marker(&self, name: &str, len: usize) -> String {
        let value = marker(len);
        self.markers
            .borrow_mut()
            .push((name.to_string(), value.clone().into_bytes()));
        value
    }

    fn track(&self, name: &str, value: &str) {
        self.markers
            .borrow_mut()
            .push((name.to_string(), value.as_bytes().to_vec()));
    }

    fn machine(&self) -> &Machine {
        &self.setup.machine
    }

    fn root(&self) -> &Path {
        &self.setup.machine.root
    }

    /// Restarts into the current deployment and runs its boot's decrypt step; /run starts empty, as at a boot.
    fn boot(&self) -> (bool, String) {
        let current = self.machine().current().unwrap();
        self.machine().reboot(Some(&format!("archix-{current}.conf")));
        let nm = self
            .root()
            .join("run/NetworkManager/system-connections/home.nmconnection");
        let nm_dir = nm.parent().unwrap().to_path_buf();
        for mounted in [nm, nm_dir, self.root().join("run/archix/secrets")] {
            while mounted_type(&mounted).is_some() {
                assert!(Command::new("umount").arg(&mounted).status().unwrap().success());
            }
        }
        let _ = fs::remove_dir_all(self.root().join("run/NetworkManager"));
        self.activate()
    }

    fn activate(&self) -> (bool, String) {
        let root = self.root().display().to_string();
        self.run(&["secrets", "activate", "--root", &root], &[], None)
    }

    fn run_secret(&self, name: &str) -> PathBuf {
        self.root().join("run/archix/secrets").join(name)
    }

    /// The sweep: every marker so far must be only where `allowed` says it may be.
    fn sweep(&self, c: &Checks, scenario: &str, allowed: &dyn Fn(&str, &str) -> bool, extra: &[&Path]) {
        let hits = leak_sweep(
            self.machine(),
            &self.setup.config,
            extra,
            &self.markers.borrow(),
            &self.outputs.borrow(),
        );
        for hit in &hits {
            c.check(
                allowed(&hit.marker, &hit.place),
                format!("{scenario}: marker {} leaked into {}", hit.marker, hit.place),
            );
        }
    }
}

/// The places a crypt_hash value may be, the exception the docs name: shadow files and the /etc merge's account texts.
fn shadow_place(place: &str) -> bool {
    place.ends_with("/etc/shadow") || place.ends_with("/etc/shadow-") || place.contains("/var/lib/archix/etc-merge/")
}

fn secrets_body(lab: &Lab, items: &str, rest: &str) -> String {
    let machine_key = archix::secrets::crypto::machine(&lab.setup.paths.state)
        .unwrap()
        .recipient;
    format!(
        r#"secrets = {{ machine_key = "{machine_key}", admins = {{ "{}" }}, items = {{ {items} }} }}, {}"#,
        lab.admin_recipient(),
        rest.trim().trim_end_matches(',')
    )
}

const ITEMS: &str = r#"
    wg0 = { file = "secrets/wg0.age" },
    api = { file = "secrets/api.age", owner = "alice", group = "alice", mode = "0440" },
    nb = { file = "secrets/nb.age" },
    nm = { file = "secrets/nm.age", path = "/run/NetworkManager/system-connections/home.nmconnection", mode = "0600" },
    pw = { file = "secrets/pw.age", kind = "crypt_hash" },
"#;

const CONSUMERS: &str = r#"
    users = { alice = { groups = { "wheel" }, hashed_password = archix.secret("pw") } },
    files = {
      ["/etc/wireguard/wg0.conf"] = { secret = "wg0" },
      ["/etc/netbird/env"] = { text = "NB_SETUP_KEY=" .. archix.secret("nb") .. "\n", mode = "0640" },
    },
    units = {
      ["archix-test-cred.service"] = "[Service]\nLoadCredential=api:" .. archix.secret_path("api") .. "\nExecStart=/bin/true\n",
      ["archix-test-env.service"] = "[Service]\nEnvironmentFile=" .. archix.secret_path("nb") .. "\nExecStart=/bin/true\n",
    },
"#;

fn mode_of(path: &Path) -> Option<(u32, u32)> {
    fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.mode() & 0o7777, meta.uid()))
}

fn uid_of(etc: &Path, user: &str) -> u32 {
    fs::read_to_string(etc.join("passwd"))
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix(&format!("{user}:x:"))?
                .split(':')
                .next()?
                .parse()
                .ok()
        })
        .unwrap()
}

fn shadow_hash(etc: &Path, user: &str) -> String {
    fs::read_to_string(etc.join("shadow"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{user}:")))
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_default()
        .to_string()
}

fn mounted_type(path: &Path) -> Option<String> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    info.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        let dash = fields.iter().position(|f| *f == "-")?;
        (fields.get(4).copied() == path.to_str()).then(|| fields[dash + 1].to_string())
    })
}

/// A fresh random crypt hash, whose bytes are a marker too.
fn hash_marker(lab: &Lab, name: &str) -> (String, String) {
    let password = marker(20);
    let hash = password::sha512_crypt(password.as_bytes(), &password::salt().unwrap());
    lab.track(name, &hash);
    (password, hash)
}

#[test]
#[ignore]
fn secrets_reach_only_the_ramfs_their_links_and_shadow_and_a_rescue_password_holds_over_a_secret_hash() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    let said = lab.ok(&["secrets", "init"]);
    c.check(
        said.contains("machine_key = \"age1") && said.contains("admin recipients are the backup"),
        format!("init prints the key and the backup advice:\n{said}"),
    );
    let state = &lab.setup.paths.state;
    c.eq(
        mode_of(&archix::secrets::crypto::identity_path(state)).map(|m| m.0),
        Some(0o400),
        "the identity is 0400",
    );
    let wg0 = format!("[Interface]\nPrivateKey = {}\n", lab.marker("wg0", 32));
    let api = lab.marker("api", 12);
    let nb = lab.marker("nb", 8);
    let nm = format!("[connection]\nid=home\n[wifi-security]\npsk={}\n", lab.marker("nm", 24));
    let (pw, pw_hash) = hash_marker(&lab, "pw");
    lab.setup.set_body(&[], &secrets_body(&lab, ITEMS, CONSUMERS));
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("doesn't exist yet") && refused.contains("archix secrets edit"),
        format!("a declared secret with no age file is refused: {refused}"),
    );
    for (name, value) in [("wg0", &wg0), ("api", &api), ("nb", &nb), ("nm", &nm)] {
        lab.set(name, value.as_bytes());
    }
    lab.set("pw", format!("{pw_hash}\n").as_bytes());
    let recorded = lab.setup.read("secrets/recipients.toml").unwrap_or_default();
    c.check(
        recorded.contains("secrets/wg0.age") && recorded.contains(&lab.admin_recipient()),
        format!("recipients.toml records who each file is encrypted to:\n{recorded}"),
    );
    let checked = lab.ok(&["secrets", "check"]);
    c.check(
        checked.contains("this machine: decrypts it") && checked.contains("/etc/wireguard/wg0.conf"),
        format!("check:\n{checked}"),
    );
    let diff = lab.ok(&["diff"]);
    c.check(
        diff.contains("secret     wg0: new (/etc/wireguard/wg0.conf)")
            && diff.contains("password   alice's comes from secret pw")
            && !diff.contains("/usr/lib/archix/secrets/"),
        format!("diff names the secrets, not their files:\n{diff}"),
    );
    let out = lab.ok(&["switch", "--noconfirm"]);
    let number = lab.machine().current().unwrap();
    let deployment = lab.machine().deployment(number);
    c.eq(
        fs::read_link(deployment.join("etc/wireguard/wg0.conf")).ok(),
        Some(PathBuf::from("/run/archix/secrets/wg0")),
        format!("a secret file is a link in the deployment:\n{out}"),
    );
    let template = archix::secrets::template_id("/etc/netbird/env");
    c.eq(
        fs::read_link(deployment.join("etc/netbird/env")).ok(),
        Some(PathBuf::from(format!("/run/archix/secrets/files/{template}"))),
        "a template is a link too",
    );
    c.check(
        fs::read_to_string(deployment.join("etc/systemd/system/archix-test-cred.service"))
            .unwrap_or_default()
            .contains("LoadCredential=api:/run/archix/secrets/api"),
        "units read secret_path",
    );
    c.check(
        deployment.join("usr/lib/archix/secrets/manifest.json").is_file()
            && deployment.join("etc/systemd/system/archix-secrets.service").is_file()
            && deployment
                .join("etc/systemd/system/sysinit.target.wants/archix-secrets.service")
                .is_symlink(),
        "the deployment has its manifest and the boot unit",
    );
    c.check(
        !shadow_hash(&deployment.join("etc"), "alice").starts_with("$6$"),
        "the switch sets no secret hash: the boot does",
    );

    let (activated, said) = lab.boot();
    c.check(activated, format!("the boot's decrypt step: {said}"));
    let run = lab.root().join("run/archix/secrets");
    c.eq(
        mounted_type(&run),
        Some("ramfs".to_string()),
        "the secrets are on a ramfs",
    );
    c.eq(mode_of(&run).map(|m| m.0), Some(0o711), "its names can't be listed");
    let etc = lab.root().join("etc");
    let alice = uid_of(&etc, "alice");
    c.eq(
        fs::read_to_string(lab.run_secret("wg0")).ok(),
        Some(wg0.clone()),
        "wg0's value",
    );
    c.eq(mode_of(&lab.run_secret("wg0")), Some((0o400, 0)), "wg0 is root's, 0400");
    c.eq(
        mode_of(&lab.run_secret("api")),
        Some((0o440, alice)),
        "api is alice's, 0440",
    );
    c.eq(
        fs::read_to_string(run.join("files").join(&template)).ok(),
        Some(format!("NB_SETUP_KEY={nb}\n")),
        "the template rendered",
    );
    c.eq(
        mode_of(&run.join("files").join(&template)),
        Some((0o640, 0)),
        "the template's mode",
    );
    let nm_path = lab
        .root()
        .join("run/NetworkManager/system-connections/home.nmconnection");
    c.eq(fs::read_to_string(&nm_path).ok(), Some(nm.clone()), "a path under /run");
    c.eq(mode_of(&nm_path), Some((0o600, 0)), "the path's mode");
    c.eq(
        mounted_type(nm_path.parent().unwrap()),
        Some("ramfs".to_string()),
        "the path's directory is a ramfs, so an app's save by rename stays there",
    );
    let saved = nm_path.with_extension("tmp");
    fs::write(&saved, format!("{nm}# edited\n")).unwrap();
    c.check(
        fs::rename(&saved, &nm_path).is_ok(),
        "an app saving the profile by renaming a new file over it succeeds",
    );
    c.eq(
        shadow_hash(&etc, "alice"),
        pw_hash.clone(),
        "alice's password from the secret",
    );
    c.eq(
        password::verify(pw.as_bytes(), &shadow_hash(&etc, "alice")),
        Some(true),
        "alice logs in with it",
    );
    let status = lab.ok(&["status"]);
    c.check(
        status.contains("the password hashes of alice come from secrets")
            && !status.contains("is missing at this boot"),
        format!("status:\n{status}"),
    );
    lab.ok(&["cache", "push", "/tmp/archix-secrets-cache"]);
    lab.sweep(
        &c,
        "switch and boot",
        &|marker, place| {
            (marker == "pw" && shadow_place(place)) || (marker == "nm" && place.contains("/run/NetworkManager/"))
        },
        &[Path::new("/tmp/archix-secrets-cache")],
    );
    let _ = fs::remove_dir_all("/tmp/archix-secrets-cache");

    lab.machine().power_off();
    let root = lab.root().display().to_string();
    let esp = lab.machine().esp().display().to_string();
    let rescued = lab.setup.answering(
        "rescued pass\nrescued pass\n",
        &["rescue", "--root-dir", &root, "--esp-dir", &esp, "password", "alice"],
    );
    c.check(
        rescued.contains("boots keep it over the crypt_hash secret pw"),
        format!("the rescue records the override: {rescued}"),
    );
    let (_, said) = lab.boot();
    c.eq(
        password::verify(b"rescued pass", &shadow_hash(&etc, "alice")),
        Some(true),
        format!("the boot after the rescue keeps its password: {said}"),
    );
    let notes = fs::read_to_string(lab.root().join("var/lib/archix/notes")).unwrap_or_default();
    c.check(
        notes.contains("alice's password was set in the rescue entry"),
        format!("a login note says so:\n{notes}"),
    );
    lab.set("wg0", format!("{wg0}# same key, rewritten\n").as_bytes());
    let out = lab.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("keeping alice's password set in the rescue entry over the crypt_hash secret pw"),
        format!("an unchanged secret keeps it:\n{out}"),
    );
    lab.boot();
    c.eq(
        password::verify(b"rescued pass", &shadow_hash(&etc, "alice")),
        Some(true),
        "and the boot after that switch",
    );
    let (_, new_hash) = hash_marker(&lab, "pw2");
    lab.set("pw", new_hash.as_bytes());
    let out = lab.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("secret pw changed since the rescue entry set alice's password"),
        format!("a changed secret replaces it:\n{out}"),
    );
    lab.boot();
    c.eq(
        shadow_hash(&etc, "alice"),
        new_hash,
        "the new deployment's boot sets the new hash",
    );
    lab.sweep(
        &c,
        "rescue override",
        &|marker, place| {
            ((marker == "pw" || marker == "pw2") && shadow_place(place))
                || (marker == "nm" && place.contains("/run/NetworkManager/"))
        },
        &[],
    );
    c.finish();
}

#[test]
#[ignore]
fn a_secret_this_machine_cant_open_is_refused_unless_left_out_and_then_counts_for_no_login() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    lab.ok(&["secrets", "init"]);
    const BOB: &str =
        "$6$7NaGYGLemRAuL7Ox$CWh388jGyxO3sNyFQ0DALQEpj8Fc8AiKYTtWx./.uAa2zzfv6LqWJApTfEeTpr6zlVEJdRTBs/aAjCAONSPsP0";
    lab.setup.set_body(
        &[],
        &format!(r#"users = {{ bob = {{ groups = {{ "wheel" }}, hashed_password = "{BOB}" }} }}"#),
    );
    lab.ok(&["switch", "--noconfirm"]);
    lab.boot();
    let items = r#"pw = { file = "secrets/pw.age", kind = "crypt_hash" }, other = { file = "secrets/other.age" }"#;
    let consumers = r#"users = { bob = { groups = { "wheel" }, hashed_password = "!" }, alice = { groups = { "wheel" }, hashed_password = archix.secret("pw") } }, files = { ["/etc/archix-other"] = { secret = "other" } }"#;
    lab.setup.set_body(&[], &secrets_body(&lab, items, consumers));
    let (_, hash) = hash_marker(&lab, "pw");
    lab.set("pw", hash.as_bytes());
    let other = lab.marker("other", 16);
    let ciphertext = archix::secrets::crypto::encrypt(&[lab.admin_recipient()], other.as_bytes()).unwrap();
    fs::write(lab.setup.config.join("secrets/other.age"), ciphertext).unwrap();
    let before = (lab.machine().current(), lab.machine().deployments());
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("secret other isn't encrypted to this machine")
            && refused.contains("--without-secrets")
            && refused.contains("nothing was changed"),
        refused.to_string(),
    );
    c.eq(
        (lab.machine().current(), lab.machine().deployments()),
        before.clone(),
        "nothing changed",
    );
    c.eq(lab.machine().leftovers(), Vec::<String>::new(), "no staging left");
    let machine_key = archix::secrets::crypto::machine(&lab.setup.paths.state)
        .unwrap()
        .recipient;
    let for_machine = archix::secrets::crypto::encrypt(&[machine_key], other.as_bytes()).unwrap();
    let for_admin = fs::read(lab.setup.config.join("secrets/other.age")).unwrap();
    fs::write(lab.setup.config.join("secrets/other.age"), &for_machine).unwrap();
    let out = lab.ok(&["switch", "--noconfirm"]);
    c.check(
        !out.contains("no account can log in"),
        format!("bob is locked and alice has no password yet, but her checked secret hash, set at the boot, is a login:\n{out}"),
    );
    let pending = (lab.machine().current(), lab.machine().deployments());
    let refused = lab.fails(&["switch", "--noconfirm", "--without-secrets"]);
    c.check(
        refused.contains("leaves no account that can log in with a password"),
        format!("without the secrets, alice's secret hash counts for no login: {refused}"),
    );
    c.eq(
        (lab.machine().current(), lab.machine().deployments()),
        pending,
        "nothing changed",
    );
    fs::write(lab.setup.config.join("secrets/other.age"), &for_admin).unwrap();
    lab.setup.set_body(
        &[],
        &secrets_body(&lab, items, &format!("{consumers}, allow_no_password_login = true")),
    );
    let out = lab.ok(&["switch", "--noconfirm", "--without-secrets"]);
    c.check(
        out.contains("--without-secrets: this secret can't be decrypted on this machine") && out.contains("other"),
        out.to_string(),
    );
    let number = lab.machine().current().unwrap();
    let manifest = archix::secrets::Manifest::read(&lab.machine().deployment(number)).unwrap();
    c.check(
        manifest.secrets["other"].absent && !manifest.secrets["pw"].absent,
        "only the unopenable one is left out",
    );
    let (activated, said) = lab.boot();
    c.check(
        !activated && said.contains("secret other wasn't decrypted"),
        said.to_string(),
    );
    c.check(!lab.run_secret("other").exists(), "nothing was written for it");
    let status = lab.ok(&["status"]);
    c.check(
        status.contains("secret other is missing at this boot") && status.contains("--without-secrets"),
        format!("status:\n{status}"),
    );
    lab.setup.set_body(&[], &secrets_body(&lab, items, consumers));
    fs::write(lab.setup.config.join("secrets/other.age"), &for_machine).unwrap();
    lab.ok(&["switch", "--noconfirm"]);
    lab.boot();
    c.eq(
        fs::read_to_string(lab.run_secret("other")).ok(),
        Some(other.clone()),
        "decrypted now",
    );
    lab.sweep(
        &c,
        "without secrets",
        &|marker, place| marker == "pw" && shadow_place(place),
        &[],
    );
    c.finish();
}

#[test]
#[ignore]
fn each_deployment_boots_its_own_values_a_rebuilt_rollback_needs_no_identity_and_a_rotated_key_retires_until_gc() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    lab.ok(&["secrets", "init"]);
    let items = r#"wg0 = { file = "secrets/wg0.age" }"#;
    let files = |n: u32| {
        format!(
            r#"files = {{ ["/etc/wireguard/wg0.conf"] = {{ secret = "wg0" }}, ["/etc/archix-gen"] = {{ text = "{n}" }} }}"#
        )
    };
    let v1 = lab.marker("v1", 32);
    lab.setup.set_body(&[], &secrets_body(&lab, items, &files(1)));
    lab.set("wg0", v1.as_bytes());
    lab.ok(&["switch", "--noconfirm"]);
    let first = lab.machine().current().unwrap();
    let v2 = lab.marker("v2", 32);
    lab.set("wg0", v2.as_bytes());
    let diff = lab.ok(&["diff"]);
    c.check(
        diff.contains("secret     wg0: changed"),
        format!("diff tells a new value:\n{diff}"),
    );
    let vmlck = tempfile::NamedTempFile::new().unwrap();
    let (switched, out) = lab.run(
        &["switch", "--noconfirm"],
        &[("ARCHIX_TEST_VMLCK", vmlck.path().to_str().unwrap())],
        None,
    );
    let locked: u64 = fs::read_to_string(vmlck.path())
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    c.check(
        switched && locked > 4096,
        format!("the switch decrypts and scans with every page locked: VmLck {locked} kB\n{out}"),
    );
    let second = lab.machine().current().unwrap();
    lab.boot();
    c.eq(
        fs::read_to_string(lab.run_secret("wg0")).ok(),
        Some(v2.clone()),
        "deployment 2's value",
    );
    lab.ok(&["rollback", &first.to_string(), "--noconfirm"]);
    lab.boot();
    c.eq(
        fs::read_to_string(lab.run_secret("wg0")).ok(),
        Some(v1.clone()),
        "a kept rollback boots its own value",
    );
    lab.ok(&["rollback", &second.to_string(), "--noconfirm"]);
    lab.boot();
    lab.setup.set_body(
        &[],
        &secrets_body(&lab, items, &format!("{}, deployments = {{ keep = 1 }}", files(3))),
    );
    lab.ok(&["switch", "--noconfirm"]);
    lab.boot();
    c.check(
        !lab.machine().deployments().contains(&first),
        "keep = 1 dropped the first deployment",
    );
    let identity = archix::secrets::crypto::identity_path(&lab.setup.paths.state);
    let away = identity.with_extension("away");
    fs::rename(&identity, &away).unwrap();
    let rolled = lab.run(&["rollback", &first.to_string(), "--noconfirm"], &[], None);
    fs::rename(&away, &identity).unwrap();
    c.check(
        rolled.0,
        format!("rebuilding a dropped generation needs no identity:\n{}", rolled.1),
    );
    lab.boot();
    c.eq(
        fs::read_to_string(lab.run_secret("wg0")).ok(),
        Some(v1.clone()),
        "a rebuilt rollback boots its own value",
    );
    lab.sweep(&c, "rollbacks", &|_, _| false, &[]);

    lab.ok(&["rollback", &second.to_string(), "--noconfirm"]);
    lab.boot();
    let old = archix::secrets::crypto::machine(&lab.setup.paths.state)
        .unwrap()
        .recipient;
    let rotated = lab.ok(&["secrets", "rotate-machine-key"]);
    let new = archix::secrets::crypto::machine(&lab.setup.paths.state)
        .unwrap()
        .recipient;
    c.check(new != old && rotated.contains(&new), rotated.to_string());
    let (activated, _) = lab.activate();
    c.check(activated, "the retired key still opens the booted deployment's secrets");
    lab.setup.set_body(
        &[],
        &secrets_body(&lab, items, &format!("{}, deployments = {{ keep = 2 }}", files(4))),
    );
    lab.ok(&["secrets", "rekey", "--machine"]);
    lab.ok(&["switch", "--noconfirm"]);
    let gc = lab.ok(&["gc"]);
    c.check(
        !gc.contains("deleted the retired secrets identity")
            && !archix::secrets::crypto::retired(&lab.setup.paths.state).is_empty(),
        format!("a kept deployment still needs the retired key:\n{gc}"),
    );
    lab.boot();
    c.eq(
        fs::read_to_string(lab.run_secret("wg0")).ok(),
        Some(v2.clone()),
        "rekeyed to the new key",
    );
    lab.setup.set_body(
        &[],
        &secrets_body(
            &lab,
            items,
            &format!(
                "{}, deployments = {{ keep = 1 }}, generations = {{ keep = 1, keep_days = 0 }}",
                files(5)
            ),
        ),
    );
    let out = lab.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains(&format!("deleted the retired secrets identity {old}")),
        format!("once nothing kept needs it, gc drops it:\n{out}"),
    );
    c.check(
        archix::secrets::crypto::retired(&lab.setup.paths.state).is_empty(),
        "no retired identity is left",
    );
    lab.sweep(&c, "rotation", &|_, _| false, &[]);
    c.finish();
}

#[test]
#[ignore]
fn a_plaintext_file_at_a_secrets_path_goes_only_when_it_holds_the_value_and_eject_drops_or_materializes() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    lab.ok(&["secrets", "init"]);
    let wg0 = lab.marker("wg0", 32);
    let other = lab.marker("other", 24);
    let live = lab.root().join("etc/wireguard/wg0.conf");
    fs::create_dir_all(live.parent().unwrap()).unwrap();
    fs::write(&live, &other).unwrap();
    lab.setup.set_body(
        &[],
        &secrets_body(
            &lab,
            r#"wg0 = { file = "secrets/wg0.age", mode = "0600" }"#,
            r#"files = { ["/etc/wireguard/wg0.conf"] = { secret = "wg0" } }"#,
        ),
    );
    lab.set("wg0", wg0.as_bytes());
    let before = (lab.machine().current(), lab.machine().deployments());
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("holds something other than secret wg0") && refused.contains("nothing was changed"),
        refused.to_string(),
    );
    c.eq(
        (lab.machine().current(), lab.machine().deployments()),
        before.clone(),
        "nothing changed",
    );
    fs::write(&live, &wg0).unwrap();
    let out = lab.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("replacing  /etc/wireguard/wg0.conf, which holds secret wg0's value"),
        out.to_string(),
    );
    c.check(
        !Path::new("/var/lib/archix/backups/etc/wireguard/wg0.conf").exists(),
        "no backup of what held the value",
    );
    let replaced = before.0.unwrap();
    lab.boot();
    lab.sweep(
        &c,
        "an existing file",
        &|marker, place| marker == "wg0" && place.ends_with(&format!("/deploy/{replaced}/etc/wireguard/wg0.conf")),
        &[],
    );

    fs::remove_file(&live).unwrap();
    fs::write(&live, &wg0).unwrap();
    let status = lab.ok(&["status"]);
    c.check(
        status.contains("/etc/wireguard/wg0.conf should be a link to secret wg0 but is a file"),
        format!("status warns of a link someone replaced:\n{status}"),
    );
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("should be a link to secret wg0 but is a file") && refused.contains("nothing was changed"),
        refused.to_string(),
    );
    fs::remove_file(&live).unwrap();
    std::os::unix::fs::symlink("/run/archix/secrets/wg0", &live).unwrap();

    let refused = lab.fails(&["eject", "--noconfirm"]);
    c.check(
        refused.contains("--secrets drop") && refused.contains("--secrets materialize"),
        format!("eject asks what becomes of the secrets: {refused}"),
    );
    let out = lab.ok(&["eject", "--noconfirm", "--secrets", "materialize"]);
    c.eq(
        fs::read_to_string(&live).ok(),
        Some(wg0.clone()),
        format!("materialized as a real file:\n{out}"),
    );
    c.eq(mode_of(&live), Some((0o600, 0)), "with the secret's mode and owner");
    c.check(
        archix::secrets::crypto::identity_path(&lab.setup.paths.state).exists(),
        "the identity stays",
    );
    lab.sweep(
        &c,
        "eject materialize",
        &|marker, place| marker == "wg0" && place == lab.root().join("etc/wireguard/wg0.conf").display().to_string(),
        &[],
    );
    c.finish();
}

#[test]
#[ignore]
fn eject_drops_the_links_and_editing_never_leaves_plain_text_or_a_torn_age_file() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    lab.ok(&["secrets", "init"]);
    let v1 = lab.marker("v1", 32);
    let v2 = lab.marker("v2", 32);
    let v3 = lab.marker("v3", 32);
    lab.setup.set_body(
        &[],
        &secrets_body(
            &lab,
            r#"wg0 = { file = "secrets/wg0.age" }"#,
            r#"files = { ["/etc/wireguard/wg0.conf"] = { secret = "wg0" } }"#,
        ),
    );
    lab.set("wg0", v1.as_bytes());
    let age = lab.setup.config.join("secrets/wg0.age");
    let original = fs::read(&age).unwrap();
    let (killed, _) = lab.run(
        &["secrets", "set", "wg0"],
        &[("ARCHIX_TEST_CRASH", "secrets-write")],
        Some(v2.as_bytes()),
    );
    c.check(!killed, "the set was killed before its rename");
    c.eq(
        fs::read(&age).unwrap(),
        original.clone(),
        "a killed set leaves the old age file",
    );
    let scripts = tempfile::tempdir_in("/var/tmp").unwrap();
    let editor = scripts.path().join("edit");
    write_executable(&editor, "#!/bin/sh\nprintf %s \"$ARCHIX_TEST_VALUE\" > \"$1\"\n");
    let slow = scripts.path().join("slow");
    write_executable(
        &slow,
        "#!/bin/sh\necho \"$$ $1\" > \"$ARCHIX_TEST_SEEN\"\nexec sleep 60\n",
    );
    let seen = scripts.path().join("seen");
    let mut editing = lab.command(
        &["secrets", "edit", "wg0"],
        &[
            ("EDITOR", slow.to_str().unwrap()),
            ("ARCHIX_TEST_SEEN", seen.to_str().unwrap()),
        ],
    );
    let child = editing.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    for _ in 0..600 {
        if seen.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let seen = fs::read_to_string(&seen).unwrap_or_default();
    let (editor_pid, scratch) = seen.trim().split_once(' ').unwrap_or_default();
    let in_editors_view = Path::new("/proc")
        .join(editor_pid)
        .join("root")
        .join(scratch.trim_start_matches('/'));
    c.check(
        in_editors_view.is_file(),
        format!("the editor has the plain text at {scratch}"),
    );
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let output = child.wait_with_output().unwrap();
    lab.keep(&output);
    c.check(
        !in_editors_view.exists(),
        format!("the plain text {scratch} is gone after a kill, though the editor still runs"),
    );
    if let Ok(pid) = editor_pid.parse::<i32>()
        && pid > 1
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    c.eq(
        fs::read(&age).unwrap(),
        original.clone(),
        "a killed edit leaves the old age file",
    );
    let (ok, text) = lab.run(
        &["secrets", "edit", "wg0"],
        &[
            ("EDITOR", editor.to_str().unwrap()),
            ("ARCHIX_TEST_VALUE", &v2),
            ("ARCHIX_TEST_CRASH", "secrets-write"),
        ],
        None,
    );
    c.check(!ok, format!("an edit killed at its write: {text}"));
    c.eq(fs::read(&age).unwrap(), original.clone(), "leaves the old age file");
    let disk = Path::new("/var/tmp/archix-secrets-on-disk");
    let _ = fs::remove_dir_all(disk);
    fs::create_dir_all(disk).unwrap();
    let (ok, text) = lab.run(
        &["secrets", "edit", "wg0"],
        &[
            ("EDITOR", editor.to_str().unwrap()),
            ("ARCHIX_TEST_VALUE", &v2),
            ("ARCHIX_TEST_SECRETS_SCRATCH", disk.to_str().unwrap()),
        ],
        None,
    );
    c.check(
        !ok && text.contains("isn't a tmpfs or ramfs") && fs::read_dir(disk).unwrap().next().is_none(),
        format!("editing on a disk is refused: {text}"),
    );
    let _ = fs::remove_dir_all(disk);
    let (ok, text) = lab.run(
        &["secrets", "edit", "wg0"],
        &[("EDITOR", editor.to_str().unwrap()), ("ARCHIX_TEST_VALUE", &v3)],
        None,
    );
    c.check(
        ok && text.contains("encrypted secret wg0"),
        format!("an edit through the admin's key: {text}"),
    );
    let (ok, text) = lab.run(
        &["secrets", "edit", "wg0", "--machine"],
        &[("EDITOR", editor.to_str().unwrap()), ("ARCHIX_TEST_VALUE", &v3)],
        None,
    );
    c.check(
        ok && text.contains("unchanged"),
        format!("an edit that changes nothing writes nothing: {text}"),
    );
    lab.ok(&["switch", "--noconfirm"]);
    lab.boot();
    c.eq(
        fs::read_to_string(lab.run_secret("wg0")).ok(),
        Some(v3.clone()),
        "the edited value",
    );
    let out = lab.ok(&["eject", "--noconfirm", "--secrets", "drop"]);
    let live = lab.root().join("etc/wireguard/wg0.conf");
    c.check(
        live.symlink_metadata().is_err() && out.contains("plain Arch now"),
        format!("the link is gone:\n{out}"),
    );
    c.check(
        !lab.root().join("etc/systemd/system/archix-secrets.service").exists(),
        "the boot unit is gone",
    );
    lab.sweep(&c, "editing and eject drop", &|_, _| false, &[]);
    c.finish();
}

#[test]
#[ignore]
fn a_value_copied_into_the_initramfs_fails_the_switch_and_a_short_one_in_etc_only_warns() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    lab.ok(&["secrets", "init"]);
    let pin = lab.marker("pin", 12);
    let copy = lab.root().join("etc/archix-test-pin");
    fs::write(&copy, format!("{pin}\n")).unwrap();
    let mkinitcpio = lab.root().join("etc/mkinitcpio.conf");
    let original = fs::read_to_string(&mkinitcpio).unwrap();
    fs::write(
        &mkinitcpio,
        original.replace("FILES=()", "FILES=(/etc/archix-test-pin)"),
    )
    .unwrap();
    lab.setup.set_body(
        &[],
        &secrets_body(
            &lab,
            r#"pin = { file = "secrets/pin.age" }"#,
            r#"files = { ["/etc/archix-pin"] = { secret = "pin" } }"#,
        ),
    );
    lab.set("pin", pin.as_bytes());
    let before = (lab.machine().current(), lab.machine().deployments());
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("secret pin in /boot/") && refused.contains("nothing was changed") && !refused.contains(&pin),
        format!("a short value in a UKI fails the switch:\n{refused}"),
    );
    c.eq(
        (lab.machine().current(), lab.machine().deployments()),
        before,
        "nothing changed",
    );
    c.eq(lab.machine().leftovers(), Vec::<String>::new(), "no staging left");
    fs::write(&mkinitcpio, &original).unwrap();
    let out = lab.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("secret pin's value, under 16 bytes, also occurs in /etc/archix-test-pin"),
        format!("in /etc a short one warns:\n{out}"),
    );
    let long = lab.marker("long", 20);
    lab.setup.set_body(
        &[],
        &secrets_body(
            &lab,
            r#"pin = { file = "secrets/pin.age" }"#,
            &format!(r#"files = {{ ["/etc/archix-pin"] = {{ secret = "pin" }}, ["/etc/archix-plain"] = {{ text = "{long}" }} }}"#),
        ),
    );
    lab.set("pin", long.as_bytes());
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("secret pin in /etc/archix-plain") && !refused.contains(&long),
        format!("a value written into a managed file is refused:\n{refused}"),
    );
    lab.setup.set_body(
        &[],
        &secrets_body(
            &lab,
            r#"pin = { file = "secrets/pin.age" }"#,
            r#"files = { ["/etc/archix-pin"] = { secret = "pin" } }"#,
        ),
    );
    let saved = lab
        .root()
        .join("etc/NetworkManager/system-connections/home.nmconnection");
    fs::create_dir_all(saved.parent().unwrap()).unwrap();
    fs::write(&saved, format!("[wireguard]\nprivate-key={long}\n")).unwrap();
    let refused = lab.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("secret pin in /etc/NetworkManager/system-connections/home.nmconnection")
            && refused.contains("nothing was changed")
            && !refused.contains(&long),
        format!("a profile NetworkManager saved to /etc with the value in it fails the next switch:\n{refused}"),
    );
    fs::remove_file(&saved).unwrap();
    lab.ok(&["switch", "--noconfirm"]);
    fs::remove_file(&copy).unwrap();
    lab.sweep(
        &c,
        "initramfs",
        &|marker, place| {
            (marker == "pin" && place.ends_with("/etc/archix-test-pin"))
                || (marker == "long" && place.ends_with("init.lua"))
        },
        &[],
    );
    c.finish();
}

#[test]
#[ignore]
fn secrets_are_refused_where_they_cant_go_and_the_boot_names_what_it_couldnt_open() {
    let _system = lock_system();
    let c = Checks::default();
    let lab = Lab::new(&[]);
    lab.ok(&["secrets", "init"]);
    let items = r#"a = { file = "secrets/a.age" }, b = { file = "secrets/b.age" }"#;
    for (rest, says) in [
        (
            r#"files = { ["/etc/a"] = { text = archix.secret("a") } }, hostname = archix.secret("a")"#,
            "hostname: a secret can't go here",
        ),
        (r#"units = { ["x.service"] = archix.secret("a") }"#, "LoadCredential="),
        (
            r#"hooks = { h = { run = "echo " .. archix.secret("a") } }"#,
            "hooks.h.run",
        ),
        (r#"files = { ["/etc/x"] = { secret = "nope" } }"#, "doesn't declare"),
    ] {
        lab.setup.set_body(&[], &secrets_body(&lab, items, rest));
        let refused = lab.fails(&["diff"]);
        c.check(refused.contains(says), format!("{rest}: {refused}"));
    }
    let machine_key = archix::secrets::crypto::machine(&lab.setup.paths.state)
        .unwrap()
        .recipient;
    lab.setup.set_body(
        &[],
        &format!(r#"secrets = {{ machine_key = "{machine_key}", items = {{ {items} }} }}, files = {{ ["/etc/a"] = {{ secret = "a" }} }}"#),
    );
    let refused = lab.fails(&["diff"]);
    c.check(
        refused.contains("secrets.admins is empty") && refused.contains("without_admin = true"),
        format!("no admin needs an acknowledgement: {refused}"),
    );
    lab.setup.set_body(
        &[],
        &format!(
            r#"secrets = {{ machine_key = "{machine_key}", without_admin = true, items = {{ {items} }} }}, files = {{ ["/etc/a"] = {{ secret = "a" }}, ["/etc/b"] = {{ secret = "b" }} }}"#
        ),
    );
    let a = lab.marker("a", 32);
    let b = lab.marker("b", 32);
    lab.set("a", a.as_bytes());
    lab.set("b", b.as_bytes());
    lab.ok(&["switch", "--noconfirm"]);
    let number = lab.machine().current().unwrap();
    let manifest = archix::secrets::Manifest::read(&lab.machine().deployment(number)).unwrap();
    let damaged = lab
        .machine()
        .deployment(number)
        .join(format!("usr/lib/archix/secrets/{}.age", manifest.secrets["b"].sha256));
    lab.machine().store_writable(true);
    let mut bytes = fs::read(&damaged).unwrap();
    let at = bytes.len() - 40;
    bytes[at] ^= 0x20;
    fs::write(&damaged, bytes).unwrap();
    lab.machine().store_writable(false);
    let (ok, said) = lab.boot();
    c.check(
        !ok && said.contains("secret b wasn't decrypted") && !said.contains(&b),
        format!("a damaged ciphertext is named: {said}"),
    );
    c.eq(
        fs::read_to_string(lab.run_secret("a")).ok(),
        Some(a.clone()),
        "the others are written",
    );
    c.check(!lab.run_secret("b").exists(), "the damaged one isn't");
    let identity = archix::secrets::crypto::identity_path(&lab.setup.paths.state);
    fs::rename(&identity, identity.with_extension("away")).unwrap();
    let (ok, said) = lab.boot();
    fs::rename(identity.with_extension("away"), &identity).unwrap();
    c.check(
        !ok && said.contains("no secret was decrypted at this boot") && !lab.run_secret("a").exists(),
        format!("without the identity, nothing is: {said}"),
    );
    let notes = fs::read_to_string(lab.root().join("var/lib/archix/notes")).unwrap_or_default();
    c.check(
        notes.contains("archix: secrets: no secret was decrypted"),
        format!("a login note:\n{notes}"),
    );
    lab.sweep(&c, "refusals and boot failures", &|_, _| false, &[]);
    c.finish();
}

/// The running system's secrets for a build to look for: a ramfs at /run/archix/secrets holding a value, and an identity in /var/lib/archix/secrets, both readable by anyone so only the build's isolation keeps them out; gone when dropped.
struct Planted {
    value: String,
    identity: String,
}

impl Planted {
    fn new() -> Planted {
        let (value, identity) = (marker(32), marker(32));
        let run = Path::new(archix::secrets::RUN_DIR);
        if mounted_type(run).is_none() {
            fs::create_dir_all(run).unwrap();
            assert!(
                Command::new("mount")
                    .args(["-t", "ramfs", "-o", "mode=0711", "ramfs"])
                    .arg(run)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(run.join("wg0"), &value).unwrap();
        fs::set_permissions(run.join("wg0"), fs::Permissions::from_mode(0o444)).unwrap();
        let state = Path::new("/var/lib/archix/secrets");
        fs::create_dir_all(state).unwrap();
        fs::set_permissions(state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(state.join("identity"), format!("AGE-SECRET-KEY-1{identity}\n")).unwrap();
        fs::set_permissions(state.join("identity"), fs::Permissions::from_mode(0o444)).unwrap();
        Planted { value, identity }
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(archix::secrets::RUN_DIR).status();
        let _ = fs::remove_dir_all("/var/lib/archix/secrets");
    }
}

/// A build step that writes what the build can see of the secrets, its environment and every process's command line into the build directory.
const PEEK: &str = r#"{ ls -la /run/archix/secrets /var/lib/archix/secrets; cat /run/archix/secrets/wg0 /run/archix/secrets/files/* /var/lib/archix/secrets/identity; env; for p in /proc/[0-9]*; do tr '\0' ' ' < $p/cmdline; echo; done; } > "$startdir/peek" 2>&1 || true"#;

fn peek_recipe(name: &str, version: &str) -> String {
    format!(
        "pkgname={name}\npkgver={version}\npkgrel=1\narch=(any)\nlicense=(MIT)\nbuild() {{\n  {PEEK}\n}}\n\
         package() {{\n  install -Dm644 \"$startdir/peek\" \"$pkgdir/usr/share/{name}/peek\"\n}}\n"
    )
}

#[test]
#[ignore]
fn aur_and_recipe_builds_see_no_secret_no_identity_and_nothing_of_either_in_their_environment_argv_or_log() {
    let _system = lock_system();
    let c = Checks::default();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg {
        build: PEEK,
        package: "install -Dm644 \"$startdir/peek\" \"$pkgdir/usr/share/archix-test-aurpeek/peek\"",
        ..TestPkg::new("archix-test-aurpeek", "1.0")
    });
    let body = |on_host: &str| format!("aur = {{ url = {:?}, build_on_host = {{ {on_host} }} }}", aur.url());
    let setup = Setup::with_files(
        &["archix-test-peek", "archix-test-aurpeek"],
        &body(r#""archix-test-aurpeek""#),
        &[(
            "pkgbuilds/archix-test-peek/PKGBUILD",
            &peek_recipe("archix-test-peek", "1.0"),
        )],
    );
    let planted = Planted::new();
    let mut outputs = Vec::new();
    let look = |what: &str, report: &str| {
        c.check(
            !report.contains(&planted.value)
                && !report.contains(&planted.identity)
                && !report
                    .lines()
                    .any(|line| line.ends_with(" wg0") || line.ends_with(" identity")),
            format!("{what} saw a secret, the identity or their names:\n{report}"),
        );
    };
    let out = setup.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("in a clean chroot") && out.contains("on the host"),
        format!("both kinds of build ran:\n{out}"),
    );
    look(
        "the recipe built in the chroot (makechrootpkg)",
        &fs::read_to_string(system_path("/usr/share/archix-test-peek/peek")).unwrap_or_default(),
    );
    look(
        "the AUR package built on the host before the layout (makepkg as the build user)",
        &fs::read_to_string(system_path("/usr/share/archix-test-aurpeek/peek")).unwrap_or_default(),
    );
    outputs.push(out);
    setup
        .machine
        .reboot(Some(&format!("archix-{}.conf", setup.machine.current().unwrap())));
    fs::write(
        setup.config.join("pkgbuilds/archix-test-peek/PKGBUILD"),
        peek_recipe("archix-test-peek", "2.0"),
    )
    .unwrap();
    setup.set_body(
        &["archix-test-peek", "archix-test-aurpeek"],
        &body(r#""archix-test-aurpeek", "archix-test-peek""#),
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    look(
        "the recipe built on the host in a build view of the laid-out system",
        &fs::read_to_string(system_path("/usr/share/archix-test-peek/peek")).unwrap_or_default(),
    );
    c.check(
        fs::read_to_string(system_path("/usr/share/archix-test-peek/peek"))
            .is_ok_and(|r| r.contains("2.0") || r.contains("PATH=")),
        "the second build ran",
    );
    outputs.push(out);
    for (i, out) in outputs.iter().enumerate() {
        look(&format!("the build log of switch {}", i + 1), out);
    }
    let markers = vec![
        ("value".to_string(), planted.value.clone().into_bytes()),
        ("identity".to_string(), planted.identity.clone().into_bytes()),
    ];
    for hit in leak_sweep(&setup.machine, &setup.config, &[], &markers, &outputs) {
        c.check(
            hit.marker == "identity" && hit.place.starts_with("/var/lib/archix/secrets/"),
            format!("builds: marker {} leaked into {}", hit.marker, hit.place),
        );
    }
    drop(planted);
    c.finish();
}
