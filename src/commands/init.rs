use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, lchown, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use archix::adopt::{Candidates, Fit, best_snapshot, fit};
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::index::Index;
use archix::aur::review::review;
use archix::capture::{self, FoundFile};
use archix::date::Date;
use archix::eval::{self, Evaluation};
use archix::installed;
use archix::integration;
use archix::lock::{HostLock, Lockfile};
use archix::luagen::Lua;
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::plan::{Change, plan};

use super::lock::{diff, save_lock};
use super::{Declared, Target, aur_git, aur_source, confirm, declared, require_root, user_cache};

/// Answers questions interactively, or with each question's default under `--defaults`.
struct Ask {
    defaults: bool,
    theme: ColorfulTheme,
}

impl Ask {
    fn yes(&self, question: &str, default: bool) -> Result<bool> {
        if self.defaults {
            return Ok(default);
        }
        Ok(Confirm::with_theme(&self.theme)
            .with_prompt(question)
            .default(default)
            .interact()?)
    }

    fn pick(&self, question: &str, items: &[&str]) -> Result<usize> {
        if self.defaults {
            return Ok(0);
        }
        Ok(Select::with_theme(&self.theme)
            .with_prompt(question)
            .items(items)
            .default(0)
            .interact()?)
    }

    fn text(&self, question: &str, default: &str) -> Result<String> {
        if self.defaults {
            return Ok(default.to_string());
        }
        Ok(Input::<String>::with_theme(&self.theme)
            .with_prompt(question)
            .default(default.to_string())
            .interact_text()?)
    }

    fn several(&self, question: &str, items: &[String], checked: &[bool]) -> Result<Vec<usize>> {
        if self.defaults || items.is_empty() {
            return Ok((0..items.len()).filter(|i| checked[*i]).collect());
        }
        Ok(MultiSelect::with_theme(&self.theme)
            .with_prompt(question)
            .items(items)
            .defaults(checked)
            .interact()?)
    }
}

/// What init read from the machine, before any question is asked.
struct Machine {
    local: BTreeMap<String, LocalPkg>,
    details: BTreeMap<String, capture::Installed>,
    explicit: Vec<String>,
    foreign: BTreeSet<String>,
    foreign_explicit: Vec<String>,
    orphans: Vec<String>,
}

/// The config being built, as the host module's top-level fields in the order they're written.
#[derive(Default)]
struct Host {
    fields: Vec<(String, Lua)>,
    packages: Vec<String>,
    copies: Vec<(PathBuf, PathBuf)>,
    home_user: Option<String>,
}

impl Host {
    fn set(&mut self, key: &str, value: Lua) {
        self.fields.push((key.to_string(), value));
    }
}

pub fn init(config_dir: &Path, host: &str, defaults: bool) -> Result<()> {
    require_root("init")?;
    if !defaults && !std::io::stdin().is_terminal() {
        bail!("`archix init` asks questions; run it in a terminal, or pass --defaults to take every default");
    }
    refuse_existing(config_dir)?;
    let ask = Ask {
        defaults,
        theme: ColorfulTheme::default(),
    };
    let paths = Paths::system();
    let dir = choose_location(&ask, config_dir)?;
    let git = which("git") && ask.yes("Make the config directory a git repository?", false)?;

    eprintln!("reading this system");
    let machine = Machine {
        local: read_local_db(&paths.local_db())?,
        details: capture::read_installed(&paths.local_db())?,
        explicit: capture::pacman_query("en")?,
        foreign: capture::pacman_query("m")?.into_iter().collect(),
        foreign_explicit: capture::pacman_query("em")?,
        orphans: capture::pacman_query("dt")?,
    };
    let mut config = Host::default();
    let mut skip: BTreeSet<String> = integration::files(&paths, &std::env::current_exe()?)
        .into_iter()
        .chain(integration::boot_files(&paths, &std::env::current_exe()?))
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();

    host_settings(&mut config, &mut skip);
    packages(&ask, &mut config, &machine)?;
    accounts(&mut config)?;
    services(&mut config)?;
    files(&ask, &mut config, &dir, host, &machine, &skip)?;
    boot(&ask, &mut config, &machine)?;
    generations(&ask, &mut config)?;
    dotfiles(&ask, &mut config, &dir)?;

    write_config(&dir, host, &config)?;
    let evaluation = evaluate(&dir, host)?;
    let mut declared = declared(&evaluation.config)?;
    let mut lock = lock_as_installed(&declared, &machine, &paths)?;
    let swapped = swapped_providers(&lock, &machine, &declared);
    let evaluation = if swapped.is_empty() {
        evaluation
    } else {
        println!(
            "declaring {} so the lock keeps the providers this machine has instead of the default ones",
            swapped.join(" ")
        );
        config.packages.extend(swapped);
        config.packages.sort();
        write_installed(&dir, host, &config.packages)?;
        let evaluation = evaluate(&dir, host)?;
        declared = super::declared(&evaluation.config)?;
        lock = lock_as_installed(&declared, &machine, &paths)?;
        evaluation
    };
    let target = Target {
        config_dir: &dir,
        host,
        config: &evaluation.config,
    };
    let git_aur = aur_git(&declared)?;
    review(&git_aur, None, &lock, declared.aur_review && !defaults, &confirm)?;
    save_lock(&target, &mut Lockfile::default(), None, &lock)?;
    hand_over(&dir)?;
    if git {
        run(Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]), "git init")?;
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "-A"]), "git add")?;
    }

    println!("\nwhat `archix switch` would change now:");
    diff(&target)?;
    println!(
        "\nthe config is in {}; read it over, then run `sudo archix switch` to make this the first generation",
        dir.display()
    );
    Ok(())
}

fn refuse_existing(dir: &Path) -> Result<()> {
    if dir.join("init.lua").exists() {
        bail!(
            "{} already has an init.lua; init only writes a new config",
            dir.display()
        );
    }
    Ok(())
}

fn which(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {what}"))?;
    if !status.success() {
        bail!("{what} failed");
    }
    Ok(())
}

/// The directory to write to; choosing another one leaves `default` as a symlink to it.
fn choose_location(ask: &Ask, default: &Path) -> Result<PathBuf> {
    let answer = ask.text(
        "Where should the config live? Any directory works, such as a clone of your own repo",
        &default.to_string_lossy(),
    )?;
    let chosen = PathBuf::from(answer.trim());
    if !chosen.is_absolute() {
        bail!("{} is not an absolute path", chosen.display());
    }
    fs::create_dir_all(&chosen).with_context(|| format!("creating {}", chosen.display()))?;
    if chosen != default {
        refuse_existing(&chosen)?;
        if fs::symlink_metadata(default).is_ok() {
            bail!(
                "{} already exists, so it can't link to {}; move it away or keep the config there",
                default.display(),
                chosen.display()
            );
        }
        symlink(&chosen, default).with_context(|| format!("linking {} to {}", default.display(), chosen.display()))?;
        println!("{} now links to {}", default.display(), chosen.display());
    }
    Ok(chosen)
}

fn exact(path: &str, content: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text == content)
}

/// Hostname, timezone, locales and keymap, but only where the option would write exactly what's there now.
fn host_settings(config: &mut Host, skip: &mut BTreeSet<String>) {
    let hostname = fs::read_to_string("/etc/hostname").unwrap_or_default();
    let hostname = hostname.trim();
    if !hostname.is_empty() && exact("/etc/hostname", &format!("{hostname}\n")) {
        config.set("hostname", Lua::str(hostname));
        skip.insert("/etc/hostname".into());
    }
    if let Ok(target) = fs::read_link("/etc/localtime")
        && let Some(zone) = target.to_string_lossy().strip_prefix("/usr/share/zoneinfo/")
    {
        config.set("timezone", Lua::str(zone));
    }
    let gen_text = fs::read_to_string("/etc/locale.gen").unwrap_or_default();
    let locales: Vec<String> = gen_text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();
    let rendered: Vec<String> = locales
        .iter()
        .map(|locale| {
            let charset = locale.split_once('.').map_or("ISO-8859-1", |(_, charset)| charset);
            format!("{locale} {charset}")
        })
        .collect();
    if let Some(first) = locales.first()
        && !gen_text.contains('#')
        && gen_text == format!("{}\n", rendered.join("\n"))
        && exact("/etc/locale.conf", &format!("LANG={first}\n"))
    {
        config.set("locales", Lua::strings(&locales));
        skip.extend(["/etc/locale.gen".to_string(), "/etc/locale.conf".to_string()]);
    }
    let vconsole = fs::read_to_string("/etc/vconsole.conf").unwrap_or_default();
    if let Some(keymap) = vconsole.strip_prefix("KEYMAP=").map(str::trim)
        && vconsole == format!("KEYMAP={keymap}\n")
    {
        config.set("keymap", Lua::str(keymap));
        skip.insert("/etc/vconsole.conf".into());
    }
}

fn packages(ask: &Ask, config: &mut Host, machine: &Machine) -> Result<()> {
    let conf = fs::read_to_string("/etc/pacman.conf").context("reading /etc/pacman.conf")?;
    let (official, custom) = capture::parse_pacman_conf(&conf, |path| fs::read_to_string(path).ok());
    if official != ["core", "extra"] {
        config.set("repos", Lua::strings(&official));
    }
    if !custom.is_empty() {
        let repos = custom
            .iter()
            .map(|(name, repo)| {
                let mut fields = vec![
                    ("name".to_string(), Lua::str(name)),
                    ("servers".to_string(), Lua::strings(&repo.servers)),
                    ("sig_level".to_string(), Lua::str(&repo.sig_level)),
                ];
                if repo.before_official {
                    fields.push(("before_official".into(), Lua::Bool(true)));
                }
                Lua::Table(fields)
            })
            .collect();
        config.set("custom_repos", Lua::List(repos));
        println!(
            "third-party repos {} are now declared; their signing keys stay in pacman's keyring, list them under `keys` to have archix import them on other machines",
            custom
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let policy = [
            ("ask", "ask each time, defaulting to falling back (recommended)"),
            ("fallback", "fall back without asking"),
            ("fail", "fail"),
        ];
        let choice = ask.pick(
            "When a locked package disappears from a third-party repo, archix can fall back to its current version, then the AUR, then the official repos",
            &policy.map(|(_, label)| label),
        )?;
        if choice > 0 {
            config.set("third_party_fallback", Lua::str(policy[choice].0));
        }
    }

    let mut ignored: Vec<String> = Vec::new();
    let mut aur_packages = Vec::new();
    if !machine.foreign.is_empty() {
        let archive = Archive::new(ARCHIVE_URL);
        let index = Index::load(&archive, archix::aur::DEFAULT_URL, &user_cache()?, false)?;
        let missing: Vec<&String> = machine
            .foreign
            .iter()
            .filter(|name| index.get(name).is_none())
            .collect();
        if !missing.is_empty() {
            println!(
                "not in any repo or the AUR, so archix leaves them alone (ignored_packages): {}",
                missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
            );
            ignored.extend(missing.into_iter().cloned());
        }
        aur_packages = machine
            .foreign_explicit
            .iter()
            .filter(|name| index.get(name).is_some())
            .cloned()
            .collect();
    }
    let orphans: Vec<&String> = machine.orphans.iter().filter(|name| !ignored.contains(name)).collect();
    if !orphans.is_empty() {
        println!(
            "installed as dependencies, but nothing needs them any more: {}",
            orphans.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
        );
        let keep = ask.pick(
            "What should happen to them?",
            &[
                "keep them, listed in ignored_packages",
                "let the first switch remove them",
            ],
        )? == 0;
        if keep {
            ignored.extend(orphans.into_iter().cloned());
        }
    }
    let strict = ask.pick(
        "Installed packages that aren't declared",
        &[
            "get removed on switch, so the config is the whole truth (recommended)",
            "are only listed",
        ],
    )? == 0;
    if !strict {
        config.set("undeclared_packages", Lua::str("warn"));
    }
    ignored.sort();
    ignored.dedup();
    if !ignored.is_empty() {
        config.set("ignored_packages", Lua::strings(&ignored));
    }
    if !aur_packages.is_empty() {
        if !ask.yes(
            "Show each AUR build recipe and ask before trusting it, when it enters the lock or changes?",
            true,
        )? {
            config.set("aur", Lua::Table(vec![("review".into(), Lua::Bool(false))]));
        }
        let missing = [
            ("ask", "ask, defaulting to the nearest newer version (recommended)"),
            ("newer", "take the nearest newer version"),
            ("older", "take the nearest older version"),
        ];
        let choice = ask.pick(
            "When a locked AUR version can't be built any more",
            &missing.map(|(_, label)| label),
        )?;
        if choice > 0 {
            config.set("missing_versions", Lua::str(missing[choice].0));
        }
    }
    config.packages = machine.explicit.iter().chain(&aur_packages).cloned().collect();
    config.packages.sort();
    config.packages.dedup();
    Ok(())
}

fn accounts(config: &mut Host) -> Result<()> {
    let accounts = capture::parse_accounts(
        &fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?,
        &fs::read_to_string("/etc/group").context("reading /etc/group")?,
    );
    if !accounts.users.is_empty() {
        let users = accounts
            .users
            .iter()
            .map(|(name, user)| {
                let mut fields = vec![("uid".to_string(), Lua::Int(user.uid.into()))];
                if let Some(group) = &user.group {
                    fields.push(("group".into(), Lua::str(group)));
                }
                if !user.groups.is_empty() {
                    fields.push(("groups".into(), Lua::strings(&user.groups)));
                }
                if user.shell != "/bin/bash" {
                    fields.push(("shell".into(), Lua::str(&user.shell)));
                }
                if let Some(home) = &user.home {
                    fields.push(("home".into(), Lua::str(home)));
                }
                if !user.description.is_empty() {
                    fields.push(("description".into(), Lua::str(&user.description)));
                }
                (name.clone(), Lua::Table(fields))
            })
            .collect();
        config.set("users", Lua::Table(users));
    }
    if !accounts.groups.is_empty() {
        let groups = accounts
            .groups
            .iter()
            .map(|(name, gid)| (name.clone(), Lua::Table(vec![("gid".into(), Lua::Int((*gid).into()))])))
            .collect();
        config.set("groups", Lua::Table(groups));
    }
    Ok(())
}

fn services(config: &mut Host) -> Result<()> {
    let changed = capture::changed_services()?;
    if !changed.is_empty() {
        config.set(
            "services",
            Lua::Table(changed.into_iter().map(|(unit, on)| (unit, Lua::Bool(on))).collect()),
        );
    }
    Ok(())
}

fn files(
    ask: &Ask,
    config: &mut Host,
    dir: &Path,
    host: &str,
    machine: &Machine,
    skip: &BTreeSet<String>,
) -> Result<()> {
    let found = capture::config_files(&machine.details, skip)?;
    if found.is_empty() {
        return Ok(());
    }
    let labels: Vec<String> = found
        .iter()
        .map(|file| {
            let why = if file.edited { "edited" } else { "written by hand" };
            let secret = if file.secret { ", may hold secrets" } else { "" };
            format!("{} ({why}{secret})", file.path)
        })
        .collect();
    let checked: Vec<bool> = found.iter().map(|file| !file.secret).collect();
    let chosen = ask.several(
        "Config files to copy into the config and manage (space toggles; files that may hold secrets start unticked)",
        &labels,
        &checked,
    )?;
    let skipped: Vec<&str> = found
        .iter()
        .enumerate()
        .filter(|(i, file)| file.secret && !chosen.contains(i))
        .map(|(_, file)| file.path.as_str())
        .collect();
    if !skipped.is_empty() {
        println!("left unmanaged because they may hold secrets: {}", skipped.join(" "));
    }
    let mut table = Vec::new();
    for i in chosen {
        let file: &FoundFile = &found[i];
        let source = format!("files/{host}{}", file.path);
        config.copies.push((PathBuf::from(&file.path), dir.join(&source)));
        let mut fields = vec![("source".to_string(), Lua::str(&source))];
        if file.mode != 0o644 {
            fields.push(("mode".into(), Lua::str(format!("{:04o}", file.mode))));
        }
        if file.owner != "root" {
            fields.push(("owner".into(), Lua::str(&file.owner)));
        }
        if file.group != "root" {
            fields.push(("group".into(), Lua::str(&file.group)));
        }
        table.push((file.path.clone(), Lua::Table(fields)));
    }
    if !table.is_empty() {
        config.set("files", Lua::Table(table));
    }
    Ok(())
}

fn boot(ask: &Ask, config: &mut Host, machine: &Machine) -> Result<()> {
    let mut boot = Vec::new();
    if capture::is_btrfs(Path::new("/")) {
        if !ask.yes(
            "/ is btrfs: take a read-only snapshot of it before every switch, as an extra recovery point?",
            true,
        )? {
            boot.push(("snapshots".to_string(), Lua::Bool(false)));
        }
    } else {
        println!(
            "note: / isn't btrfs, so archix can't snapshot it before a switch; packages, config and boot entries still roll back"
        );
    }
    if let Some(esp) = capture::systemd_boot_esp() {
        let params = capture::kernel_params(&fs::read_to_string("/proc/cmdline").unwrap_or_default());
        if !params.iter().any(|p| p.starts_with("root=")) {
            println!("note: the kernel command line has no root=, so archix leaves booting alone");
        } else if ask.yes(
            &format!("systemd-boot is installed on {esp}: let archix write a boot entry for each generation?"),
            true,
        )? {
            boot.insert(0, ("loader".to_string(), Lua::str("systemd-boot")));
            if esp != "/boot" {
                boot.push(("esp".into(), Lua::str(&esp)));
            }
            let kernel = ["linux", "linux-lts", "linux-zen", "linux-hardened"]
                .into_iter()
                .find(|kernel| machine.local.contains_key(*kernel));
            if let Some(kernel) = kernel.filter(|kernel| *kernel != "linux") {
                boot.push(("kernel".into(), Lua::str(kernel)));
            }
            let microcode: Vec<String> = ["amd-ucode.img", "intel-ucode.img"]
                .into_iter()
                .filter(|image| Path::new(&esp).join(image).is_file())
                .map(str::to_string)
                .collect();
            if !microcode.is_empty() {
                boot.push(("microcode".into(), Lua::strings(&microcode)));
            }
            config.set("kernel_params", Lua::strings(&params));
        }
    }
    if !boot.is_empty() {
        config.set("boot", Lua::Table(boot));
    }
    Ok(())
}

fn generations(ask: &Ask, config: &mut Host) -> Result<()> {
    let number = |answer: String| -> Result<i64> {
        answer
            .trim()
            .parse::<u32>()
            .map(i64::from)
            .with_context(|| format!("{answer:?} is not a number"))
    };
    let keep = number(ask.text("How many generations should gc always keep?", "10")?)?;
    let days = number(ask.text("And keep every generation younger than how many days?", "30")?)?;
    let mut fields = Vec::new();
    if keep != 10 {
        fields.push(("keep".to_string(), Lua::Int(keep)));
    }
    if days != 30 {
        fields.push(("keep_days".to_string(), Lua::Int(days)));
    }
    if !fields.is_empty() {
        config.set("generations", Lua::Table(fields));
    }
    Ok(())
}

/// Opt-in: copies chosen dotfiles into the config and declares them as live links, so editing them edits the config.
fn dotfiles(ask: &Ask, config: &mut Host, dir: &Path) -> Result<()> {
    let users: Vec<String> = match config.fields.iter().find(|(key, _)| key == "users") {
        Some((_, Lua::Table(users))) => users.iter().map(|(name, _)| name.clone()).collect(),
        _ => Vec::new(),
    };
    if users.is_empty() || !ask.yes("Bring some dotfiles into the config too?", false)? {
        return Ok(());
    }
    let labels: Vec<&str> = users.iter().map(String::as_str).collect();
    let user = users[ask.pick("Whose?", &labels)?].clone();
    let home = PathBuf::from(format!("/home/{user}"));
    let candidates = capture::dotfile_candidates(&home);
    let chosen = ask.several(
        "Which ones? They become links into the config; the originals are backed up by the first switch",
        &candidates,
        &vec![false; candidates.len()],
    )?;
    if chosen.is_empty() {
        return Ok(());
    }
    let mut files = Vec::new();
    for i in chosen {
        let relative = &candidates[i];
        let source = format!("home/{relative}");
        config.copies.push((home.join(relative), dir.join(&source)));
        files.push((
            relative.clone(),
            Lua::Table(vec![
                ("source".into(), Lua::str(&source)),
                ("live".into(), Lua::Bool(true)),
            ]),
        ));
    }
    config.set(
        "home",
        Lua::Table(vec![
            ("user".into(), Lua::str(&user)),
            ("files".into(), Lua::Table(files)),
        ]),
    );
    config.home_user = Some(user);
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).with_context(|| format!("reading {}", from.display()))?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

fn write_config(dir: &Path, host: &str, config: &Host) -> Result<()> {
    let host_dir = installed::default_dir(dir, host);
    fs::create_dir_all(&host_dir).with_context(|| format!("creating {}", host_dir.display()))?;
    fs::write(
        dir.join("init.lua"),
        "return { imports = { \"hosts.\" .. archix.hostname } }\n",
    )?;
    let body = Lua::Table(config.fields.clone()).render(0);
    fs::write(
        host_dir.join("init.lua"),
        format!("-- Written by `archix init` from this machine; edit it freely.\nreturn {body}\n"),
    )?;
    write_installed(dir, host, &config.packages)?;
    for (from, to) in &config.copies {
        copy_tree(from, to)?;
    }
    if let Some(user) = &config.home_user {
        own_tree(&dir.join("home"), &PathBuf::from(format!("/home/{user}")))?;
    }
    Ok(())
}

fn write_installed(dir: &Path, host: &str, packages: &[String]) -> Result<()> {
    let path = installed::default_dir(dir, host).join(installed::FILE);
    fs::write(&path, installed::render(packages)).with_context(|| format!("writing {}", path.display()))
}

/// Gives `dir` and everything under it to the owner of `like`, so files in a user's repo or home stay theirs.
fn own_tree(dir: &Path, like: &Path) -> Result<()> {
    let Ok(meta) = fs::metadata(like) else { return Ok(()) };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        lchown(&path, Some(meta.uid()), Some(meta.gid())).with_context(|| format!("chown {}", path.display()))?;
        if fs::symlink_metadata(&path)?.is_dir() {
            stack.extend(fs::read_dir(&path)?.flatten().map(|entry| entry.path()));
        }
    }
    Ok(())
}

/// A config in someone's own directory belongs to them, not to root who ran init.
fn hand_over(dir: &Path) -> Result<()> {
    let Some(parent) = dir.parent() else { return Ok(()) };
    if fs::metadata(parent).is_ok_and(|meta| meta.uid() != 0) {
        own_tree(dir, parent)?;
    }
    Ok(())
}

fn evaluate(dir: &Path, host: &str) -> Result<Evaluation> {
    eval::evaluate(&eval::Input {
        config_dir: dir.to_path_buf(),
        hostname: host.to_string(),
    })
    .map_err(|errors| anyhow::anyhow!("the generated config doesn't evaluate; please report this:\n{errors}"))
}

/// Locks the snapshot the installed official packages were built for, then pins every version that differs from it.
fn lock_as_installed(declared: &Declared, machine: &Machine, paths: &Paths) -> Result<HostLock> {
    let archive = Archive::new(ARCHIVE_URL);
    let newest = machine
        .details
        .iter()
        .filter(|(name, _)| !machine.foreign.contains(*name))
        .filter_map(|(_, pkg)| pkg.build_date)
        .max();
    let day = newest.map_or_else(Date::today_utc, Date::from_unix);
    let official: Vec<String> = declared
        .repos
        .iter()
        .filter(|repo| repo.custom.is_none())
        .map(|repo| repo.name.clone())
        .collect();
    eprintln!("finding the archive snapshot closest to what's installed");
    let latest = archive.latest()?;
    let date = best_snapshot(&archive, &user_cache()?, day, latest, &official, &machine.local)?;
    let dates = BTreeSet::from([date]);
    eprintln!(
        "locking {} declared packages to the snapshot that fits what's installed",
        declared.packages.len()
    );
    let git = aur_git(declared)?;
    let aur = aur_source(&git, declared, &archive, None, false);
    fit(
        &archive,
        &user_cache()?,
        paths,
        &declared.packages,
        &Candidates { dates, day },
        None,
        &machine.local,
        &declared.repos,
        Some(&aur),
        Fit::Exact,
    )
}

/// Installed dependencies the lock would replace with another provider of the same thing.
fn swapped_providers(lock: &HostLock, machine: &Machine, declared: &Declared) -> Vec<String> {
    plan(&lock.packages, &machine.local)
        .into_iter()
        .filter_map(|change| match change {
            Change::Remove { name, .. } if !declared.ignored.contains(&name) && !machine.orphans.contains(&name) => {
                Some(name)
            }
            _ => None,
        })
        .collect()
}
