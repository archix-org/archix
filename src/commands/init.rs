use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, lchown, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use archix::adopt::{Candidates, Fit, best_snapshot, fit};
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::index::Index;
use archix::aur::review::review;
use archix::capture::{self, FoundFile};
use archix::date::Date;
use archix::eval::{self, Evaluation};
use archix::flatpak::{self, LockedApp};
use archix::installed;
use archix::integration;
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::luagen::Lua;
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::plan::{Change, plan};

use super::lock::pending;
use super::{Declared, Target, aur_git, aur_source, confirm, count, declared, print_warned, require_root, user_cache};

fn section(title: &str) {
    let rule = "─".repeat(44usize.saturating_sub(title.chars().count()));
    println!("\n{}", style(format!("── {title} {rule}")).cyan().bold());
}

fn done(text: impl std::fmt::Display) {
    println!("  {} {text}", style("✓").green().bold());
}

fn note(text: impl std::fmt::Display) {
    println!("  {} {}", style("·").dim(), style(text).dim());
}

fn warn(text: impl std::fmt::Display) {
    println!("  {} {text}", style("!").yellow().bold());
}

/// Up to `max` names, then how many more there are.
fn some_of(names: &[String], max: usize) -> String {
    let shown = names.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    match names.len().saturating_sub(max) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

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
            .max_length(15)
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
    println!(
        "\n  {}  {}",
        style("archix init").bold(),
        style("describe this machine as a config").dim()
    );
    note("nothing on the system changes; init only writes the config and its lock");

    section("Config");
    let paths = Paths::system();
    let dir = choose_location(&ask, config_dir)?;
    let git = which("git") && ask.yes("Make it a git repository?", false)?;
    let machine = Machine {
        local: read_local_db(&paths.local_db())?,
        details: capture::read_installed(&paths.local_db())?,
        explicit: capture::pacman_query("en")?,
        foreign: capture::pacman_query("m")?.into_iter().collect(),
        foreign_explicit: capture::pacman_query("em")?,
        orphans: capture::pacman_query("dt")?,
    };
    let mut config = Host::default();
    let exe = std::env::current_exe()?;
    let mut skip: BTreeSet<String> = integration::files(&paths, &exe)
        .into_iter()
        .chain(integration::boot_files(&paths, &exe))
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();

    section("Packages");
    packages(&ask, &mut config, &machine)?;
    let flatpaks = flatpaks(&mut config)?;
    section("System");
    host_settings(&mut config, &mut skip);
    accounts(&mut config)?;
    services(&mut config)?;
    section("Config files");
    files(&ask, &mut config, &dir, host, &machine, &skip)?;
    section("Boot");
    boot(&ask, &mut config, &machine)?;
    section("Generations");
    generations(&ask, &mut config)?;
    dotfiles(&ask, &mut config, &dir)?;

    section("Lock");
    write_config(&dir, host, &config)?;
    let evaluation = evaluate(&dir, host)?;
    let mut declared = declared(&evaluation.config)?;
    let mut lock = lock_as_installed(&declared, &machine, &paths)?;
    let swapped = swapped_providers(&lock, &machine, &declared);
    let evaluation = if swapped.is_empty() {
        evaluation
    } else {
        note(format!(
            "declaring {} too, so the lock keeps the providers this machine has",
            some_of(&swapped, 6)
        ));
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
    lock.flatpak = flatpaks;
    let git_aur = aur_git(&declared)?;
    review(&git_aur, None, &lock, declared.aur_review && !defaults, &confirm)?;
    let mut lockfile = Lockfile::default();
    lockfile.hosts.insert(host.to_string(), lock.clone());
    lockfile.save(&dir.join(LOCK_FILE))?;
    done(format!(
        "{} locked to the {} snapshot",
        count(lock.packages.len(), "package", "packages"),
        lock.date
    ));
    if !lock.pinned.is_empty() {
        note(format!(
            "held at their installed version instead: {}",
            some_of(&lock.pinned, 8)
        ));
    }
    hand_over(&dir)?;
    if git {
        run(Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]), "git init")?;
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "-A"]), "git add")?;
    }

    section("Result");
    let (lines, warned) = pending(&target, &declared, &lock)?;
    if lines.is_empty() {
        done("the system matches the lock and the config: the first switch changes nothing");
    } else {
        warn("the first switch would still change these:");
        for line in &lines {
            println!("      {line}");
        }
    }
    print_warned(&warned);
    summary(&dir, host, &config, git);
    Ok(())
}

fn summary(dir: &Path, host: &str, config: &Host, git: bool) {
    println!("\n  Config written to {}", style(dir.display()).bold());
    let mut rows = vec![
        ("init.lua".to_string(), "loads this host"),
        (
            format!("lua/hosts/{host}/init.lua"),
            "settings, users, services and files",
        ),
        (
            format!("lua/hosts/{host}/installed.lua"),
            "packages; archix -S and -R edit it",
        ),
    ];
    if config.copies.iter().any(|(_, to)| to.starts_with(dir.join("files"))) {
        rows.push((format!("files/{host}/"), "copies of the managed config files"));
    }
    if config.home_user.is_some() {
        rows.push(("home/".to_string(), "dotfiles, linked back into the home"));
    }
    rows.push((LOCK_FILE.to_string(), "the exact package versions"));
    let width = rows.iter().map(|(path, _)| path.len()).max().unwrap_or(0);
    for (path, what) in rows {
        println!("    {path:<width$}   {}", style(what).dim());
    }
    if git {
        note("it's a git repository now; nothing is committed yet");
    }
    println!(
        "\n  Next: look it over, then run {}\n",
        style("sudo archix switch").bold()
    );
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
    if !ask.defaults {
        note(format!(
            "any directory works, such as a clone of your own repo; {} then links to it",
            default.display()
        ));
    }
    let answer = ask.text("Where should the config live?", &default.to_string_lossy())?;
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
        done(format!("{} links to {}", default.display(), chosen.display()));
    }
    Ok(chosen)
}

fn exact(path: &str, content: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text == content)
}

/// Hostname, timezone, locales and keymap, but only where the option would write exactly what's there now.
fn host_settings(config: &mut Host, skip: &mut BTreeSet<String>) {
    let mut set = Vec::new();
    let hostname = fs::read_to_string("/etc/hostname").unwrap_or_default();
    let hostname = hostname.trim();
    if !hostname.is_empty() && exact("/etc/hostname", &format!("{hostname}\n")) {
        config.set("hostname", Lua::str(hostname));
        skip.insert("/etc/hostname".into());
        set.push(format!("hostname {hostname}"));
    }
    if let Ok(target) = fs::read_link("/etc/localtime")
        && let Some(zone) = target.to_string_lossy().strip_prefix("/usr/share/zoneinfo/")
    {
        config.set("timezone", Lua::str(zone));
        set.push(format!("timezone {zone}"));
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
        set.push(format!("locales {}", locales.join(" ")));
    }
    let vconsole = fs::read_to_string("/etc/vconsole.conf").unwrap_or_default();
    if let Some(keymap) = vconsole.strip_prefix("KEYMAP=").map(str::trim)
        && vconsole == format!("KEYMAP={keymap}\n")
    {
        config.set("keymap", Lua::str(keymap));
        skip.insert("/etc/vconsole.conf".into());
        set.push(format!("keymap {keymap}"));
    }
    if !set.is_empty() {
        done(set.join(", "));
    }
}

fn packages(ask: &Ask, config: &mut Host, machine: &Machine) -> Result<()> {
    let conf = fs::read_to_string("/etc/pacman.conf").context("reading /etc/pacman.conf")?;
    let (official, custom) = capture::parse_pacman_conf(&conf, |path| fs::read_to_string(path).ok());
    if official != ["core", "extra"] {
        config.set("repos", Lua::strings(&official));
    }
    done(format!("repos {}", official.join(", ")));
    if !custom.is_empty() {
        let names: Vec<String> = custom.iter().map(|(name, _)| name.clone()).collect();
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
        done(format!("third-party repos {}", names.join(", ")));
        note("their signing keys stay in pacman's keyring; list them under `keys` to import them on other machines");
        let policy = [
            ("ask", "ask each time, defaulting to falling back (recommended)"),
            (
                "fallback",
                "fall back without asking: its current version, then the AUR, then the official repos",
            ),
            ("fail", "fail"),
        ];
        let choice = ask.pick(
            "If a locked package disappears from a third-party repo",
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
        let missing: Vec<String> = machine
            .foreign
            .iter()
            .filter(|name| index.get(name).is_none())
            .cloned()
            .collect();
        if !missing.is_empty() {
            warn(format!(
                "not in any repo or the AUR, so archix leaves them alone: {}",
                some_of(&missing, 8)
            ));
            ignored.extend(missing);
        }
        aur_packages = machine
            .foreign_explicit
            .iter()
            .filter(|name| index.get(name).is_some())
            .cloned()
            .collect();
    }
    config.packages = machine.explicit.iter().chain(&aur_packages).cloned().collect();
    config.packages.sort();
    config.packages.dedup();
    let from_aur = if aur_packages.is_empty() {
        String::new()
    } else {
        format!(", {} from the AUR", aur_packages.len())
    };
    done(format!(
        "{} declared{from_aur}",
        count(config.packages.len(), "package", "packages")
    ));

    let orphans: Vec<String> = machine
        .orphans
        .iter()
        .filter(|name| !ignored.contains(name))
        .cloned()
        .collect();
    if !orphans.is_empty() {
        note(format!(
            "{} that nothing needs any more: {}",
            count(orphans.len(), "dependency", "dependencies"),
            some_of(&orphans, 8)
        ));
        let keep = ask.pick(
            "What should happen to them?",
            &[
                "keep them (listed in ignored_packages)",
                "let the first switch remove them",
            ],
        )? == 0;
        if keep {
            ignored.extend(orphans);
        }
    }
    note("everything installed now is declared, so the first switch removes nothing");
    let strict = ask.pick(
        "Later, when you install something with plain pacman or yay",
        &[
            "switch offers to declare it, and removes it if you say no (recommended)",
            "switch leaves it installed and only lists it",
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
        if !ask.yes("Show each AUR build recipe and ask before trusting it?", true)? {
            config.set("aur", Lua::Table(vec![("review".into(), Lua::Bool(false))]));
        }
        let missing = [
            ("ask", "ask, defaulting to the nearest newer version (recommended)"),
            ("newer", "take the nearest newer version"),
            ("older", "take the nearest older version"),
        ];
        let choice = ask.pick(
            "If a locked AUR version can't be built any more",
            &missing.map(|(_, label)| label),
        )?;
        if choice > 0 {
            config.set("missing_versions", Lua::str(missing[choice].0));
        }
    }
    Ok(())
}

/// System-wide Flatpak apps, declared as they are and locked to their installed commits.
fn flatpaks(config: &mut Host) -> Result<BTreeMap<String, LockedApp>> {
    let mut apps = flatpak::installed()?;
    let remotes = flatpak::remotes()?;
    let orphaned: Vec<String> = apps
        .iter()
        .filter(|(_, app)| app.origin != "flathub" && !remotes.contains_key(&app.origin))
        .map(|(id, _)| id.clone())
        .collect();
    if !orphaned.is_empty() {
        warn(format!(
            "their remote is gone, so archix leaves these Flatpak apps alone: {}",
            some_of(&orphaned, 6)
        ));
        apps.retain(|id, _| !orphaned.contains(id));
    }
    if apps.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut ids = Vec::new();
    let mut used = BTreeMap::new();
    for (id, app) in &apps {
        if app.origin == "flathub" {
            ids.push(id.clone());
        } else {
            ids.push(format!("{}:{id}", app.origin));
            if let Some(remote) = remotes.get(&app.origin) {
                used.insert(app.origin.clone(), remote.clone());
            }
        }
    }
    let mut fields = vec![("apps".to_string(), Lua::strings(&ids))];
    if !used.is_empty() {
        let mut table: Vec<(String, Lua)> = vec![(
            "flathub".to_string(),
            Lua::Table(vec![(
                "url".into(),
                Lua::str("https://dl.flathub.org/repo/flathub.flatpakrepo"),
            )]),
        )];
        for (name, remote) in &used {
            let mut fields = vec![("url".to_string(), Lua::str(&remote.url))];
            if !remote.gpg_verify {
                fields.push(("gpg_verify".into(), Lua::Bool(false)));
            }
            table.push((name.clone(), Lua::Table(fields)));
        }
        fields.push(("remotes".into(), Lua::Table(table)));
    }
    config.set("flatpak", Lua::Table(fields));
    done(format!(
        "{}, locked to their installed commits",
        count(apps.len(), "Flatpak app", "Flatpak apps")
    ));
    Ok(apps
        .into_iter()
        .map(|(id, app)| {
            (
                id,
                LockedApp {
                    remote: app.origin,
                    reference: app.reference,
                    commit: app.commit,
                },
            )
        })
        .collect())
}

fn accounts(config: &mut Host) -> Result<()> {
    let accounts = capture::parse_accounts(
        &fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?,
        &fs::read_to_string("/etc/group").context("reading /etc/group")?,
    );
    if !accounts.users.is_empty() {
        let names: Vec<String> = accounts.users.keys().cloned().collect();
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
        done(format!(
            "{}: {}",
            count(names.len(), "user", "users"),
            some_of(&names, 6)
        ));
        note("passwords stay where they are; archix never manages them");
    }
    if !accounts.groups.is_empty() {
        let names: Vec<String> = accounts.groups.keys().cloned().collect();
        let groups = accounts
            .groups
            .iter()
            .map(|(name, gid)| (name.clone(), Lua::Table(vec![("gid".into(), Lua::Int((*gid).into()))])))
            .collect();
        config.set("groups", Lua::Table(groups));
        done(format!(
            "{}: {}",
            count(names.len(), "group", "groups"),
            some_of(&names, 6)
        ));
    }
    Ok(())
}

fn services(config: &mut Host) -> Result<()> {
    let changed = capture::changed_services()?;
    if !changed.is_empty() {
        done(format!(
            "{} that differ from their preset",
            count(changed.len(), "service", "services")
        ));
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
    if !found.secrets.is_empty() {
        note(format!(
            "{} that hold secrets stay out of the config",
            count(found.secrets.len(), "file", "files")
        ));
    }
    let found = found.files;
    if found.is_empty() {
        note("no edited or hand-written config files found");
        return Ok(());
    }
    let recommended: Vec<usize> = (0..found.len()).filter(|i| found[*i].recommended).collect();
    let others: Vec<usize> = (0..found.len()).filter(|i| !found[*i].recommended).collect();
    let label = |file: &FoundFile| {
        let why = if file.edited {
            "changed from its package's version"
        } else {
            "not from any package"
        };
        format!("{}  {}", file.path, style(format!("({why})")).dim())
    };
    if !recommended.is_empty() {
        done(format!(
            "{} worth managing:",
            count(recommended.len(), "config file", "config files")
        ));
        for i in &recommended {
            println!("      {}", label(&found[*i]));
        }
    }
    if !others.is_empty() {
        const SHOWN: usize = 12;
        note(format!(
            "{} more, mostly changed by package install scripts:",
            count(others.len(), "file", "files")
        ));
        for i in others.iter().take(SHOWN) {
            println!("      {}", style(label(&found[*i])).dim());
        }
        if others.len() > SHOWN {
            note(format!(
                "and {} more; \"let me pick\" lists them all",
                others.len() - SHOWN
            ));
        }
    }
    let mut options = Vec::new();
    if !recommended.is_empty() {
        options.push(format!("the {} worth managing (recommended)", recommended.len()));
    }
    options.push("let me pick".to_string());
    options.push("none for now".to_string());
    let labels: Vec<&str> = options.iter().map(String::as_str).collect();
    let choice = ask.pick("Which config files should archix manage?", &labels)?;
    let chosen: Vec<usize> = match (recommended.is_empty(), choice) {
        (false, 0) => recommended,
        (false, 1) | (true, 0) => {
            let labels: Vec<String> = found.iter().map(label).collect();
            let checked: Vec<bool> = found.iter().map(|file| file.recommended).collect();
            note("space toggles, enter confirms");
            ask.several("Pick the files", &labels, &checked)?
        }
        _ => Vec::new(),
    };
    let mut table = Vec::new();
    for i in &chosen {
        let file: &FoundFile = &found[*i];
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
        done(format!(
            "{} copied into files/{host}/",
            count(chosen.len(), "file", "files")
        ));
    }
    Ok(())
}

fn boot(ask: &Ask, config: &mut Host, machine: &Machine) -> Result<()> {
    let mut boot = Vec::new();
    if capture::is_btrfs(Path::new("/")) {
        if !ask.yes(
            "/ is btrfs: snapshot it before every switch, as an extra recovery point?",
            true,
        )? {
            boot.push(("snapshots".to_string(), Lua::Bool(false)));
        }
    } else {
        note("/ isn't btrfs, so there are no snapshots; packages, config and boot entries still roll back");
    }
    match capture::systemd_boot_esp() {
        None => note("no systemd-boot found, so archix leaves booting alone"),
        Some(esp) => {
            let params = capture::kernel_params(&fs::read_to_string("/proc/cmdline").unwrap_or_default());
            if !params.iter().any(|p| p.starts_with("root=")) {
                warn("the kernel command line has no root=, so archix leaves booting alone");
            } else {
                note(format!("systemd-boot is installed on {esp}"));
                if ask.yes("Let archix write a boot entry for each generation?", true)? {
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
                    done(format!("boot entries on {esp}, kernel {}", kernel.unwrap_or("linux")));
                }
            }
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
    let days = number(ask.text("And any generation younger than how many days?", "30")?)?;
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
    done(format!(
        "gc keeps the newest {keep} generations and any younger than {days} days"
    ));
    Ok(())
}

/// Opt-in: copies chosen dotfiles into the config and declares them as live links, so editing them edits the config.
fn dotfiles(ask: &Ask, config: &mut Host, dir: &Path) -> Result<()> {
    let users: Vec<String> = match config.fields.iter().find(|(key, _)| key == "users") {
        Some((_, Lua::Table(users))) => users.iter().map(|(name, _)| name.clone()).collect(),
        _ => Vec::new(),
    };
    if users.is_empty() || ask.defaults {
        return Ok(());
    }
    section("Dotfiles");
    if !ask.yes("Bring some dotfiles into the config too?", false)? {
        return Ok(());
    }
    let labels: Vec<&str> = users.iter().map(String::as_str).collect();
    let user = users[ask.pick("Whose?", &labels)?].clone();
    let home = PathBuf::from(format!("/home/{user}"));
    let candidates = capture::dotfile_candidates(&home);
    note(
        "they're copied in and linked back, so editing them edits the config; the first switch backs up the originals",
    );
    let chosen = ask.several("Which ones?", &candidates, &vec![false; candidates.len()])?;
    if chosen.is_empty() {
        return Ok(());
    }
    let mut files = Vec::new();
    for i in &chosen {
        let relative = &candidates[*i];
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
    done(format!(
        "{} of {user}'s copied into home/",
        count(chosen.len(), "dotfile", "dotfiles")
    ));
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
    note("finding the archive snapshot closest to what's installed");
    let latest = archive.latest()?;
    let date = best_snapshot(&archive, &user_cache()?, day, latest, &official, &machine.local)?;
    let dates = BTreeSet::from([date]);
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
