mod tui;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, lchown, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use console::style;

use archix::adopt::{Candidates, Fit, best_snapshot, fit};
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::index::Index;
use archix::aur::review::review;
use archix::capture::{self, Accounts, Dotfile, FoundFile};
use archix::date::Date;
use archix::eval::{self, Evaluation};
use archix::flatpak::{self, InstalledApp, LockedApp, Remote};
use archix::installed;
use archix::integration;
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::luagen::Lua;
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::plan::{Change, plan};
use archix::recipes::Recipes;
use archix::repos::Custom;

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

/// "1 file that holds a secret stays…", "3 files that hold secrets stay…".
fn secret_note(n: usize, what: &str) -> String {
    let rest = if n == 1 {
        "that holds a secret stays out of the config and isn't listed"
    } else {
        "that hold secrets stay out of the config and aren't listed"
    };
    let what = match (n, what) {
        (1, _) => what.to_string(),
        (_, "entry") => "entries".to_string(),
        _ => format!("{what}s"),
    };
    format!("{n} {what} {rest}")
}

/// Up to `max` names, then how many more there are.
fn some_of(names: &[String], max: usize) -> String {
    let shown = names.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    match names.len().saturating_sub(max) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

/// What the package database says, before any question is asked.
struct Machine {
    local: BTreeMap<String, LocalPkg>,
    details: BTreeMap<String, capture::Installed>,
    foreign: BTreeSet<String>,
}

/// How archix could manage booting on this machine.
enum BootFacts {
    NoLoader,
    NoRoot {
        esp: String,
    },
    Ready {
        esp: String,
        kernel: String,
        microcode: Vec<String>,
        params: Vec<String>,
    },
}

/// Everything init reads from the machine; the questions only decide what of it goes into the config.
struct Facts {
    machine: Machine,
    default_dir: PathBuf,
    git: bool,
    official: Vec<String>,
    custom: Vec<(String, Custom)>,
    packages: Vec<String>,
    aur_packages: Vec<String>,
    unknown: Vec<String>,
    orphans: Vec<String>,
    /// Installed as dependencies that only something's optional dependencies want; kept as dependencies via `prefer`.
    optional: Vec<String>,
    /// Enabled or disabled units whose files no package or /etc provides, so a config can't reproduce them.
    unreproducible: Vec<String>,
    settings: Vec<(String, Lua)>,
    settings_summary: Vec<String>,
    accounts: Accounts,
    services: BTreeMap<String, bool>,
    files: Vec<FoundFile>,
    secrets: usize,
    btrfs: bool,
    boot: BootFacts,
    flatpak_apps: BTreeMap<String, InstalledApp>,
    flatpak_remotes: BTreeMap<String, Remote>,
    flatpak_gone: Vec<String>,
    dotfiles: BTreeMap<String, Home>,
}

/// One user's home and the dotfiles init can offer from it.
struct Home {
    dir: PathBuf,
    offered: Vec<Dotfile>,
    secrets: usize,
}

/// The choices that shape the config; every field starts at the recommended answer.
#[derive(Debug, Clone, PartialEq)]
struct Answers {
    location: String,
    git: bool,
    /// Write managed files read-only, like the Nix store.
    read_only: bool,
    fallback: usize,
    keep_orphans: bool,
    strict: bool,
    aur_review: bool,
    missing_versions: usize,
    files: BTreeSet<usize>,
    snapshots: bool,
    boot: bool,
    /// Empty means the default; the wizard shows it as a placeholder.
    keep: String,
    keep_days: String,
    dotfiles_user: usize,
    dotfiles: BTreeSet<String>,
    settings: BTreeSet<usize>,
    users: BTreeSet<String>,
    groups: BTreeSet<String>,
    services: BTreeSet<String>,
}

const KEEP: u32 = 10;
const KEEP_DAYS: u32 = 30;

const FALLBACK: [(&str, &str); 3] = [
    ("ask", "ask each time, defaulting to falling back (recommended)"),
    (
        "fallback",
        "fall back without asking: its current version, then the AUR, then the official repos",
    ),
    ("fail", "fail"),
];

const MISSING_VERSIONS: [(&str, &str); 3] = [
    ("ask", "ask, defaulting to the nearest newer version (recommended)"),
    ("newer", "take the nearest newer version"),
    ("older", "take the nearest older version"),
];

impl Answers {
    fn recommended(facts: &Facts) -> Answers {
        Answers {
            location: String::new(),
            git: facts.git,
            read_only: false,
            fallback: 0,
            keep_orphans: true,
            strict: true,
            aur_review: true,
            missing_versions: 0,
            files: (0..facts.files.len()).filter(|i| facts.files[*i].recommended).collect(),
            snapshots: true,
            boot: matches!(facts.boot, BootFacts::Ready { .. }),
            keep: String::new(),
            keep_days: String::new(),
            dotfiles_user: 0,
            dotfiles: facts
                .dotfiles
                .values()
                .next()
                .map(|home| {
                    home.offered
                        .iter()
                        .filter(|d| d.recommended)
                        .map(|d| d.name.clone())
                        .collect()
                })
                .unwrap_or_default(),
            settings: (0..facts.settings.len()).collect(),
            users: facts.accounts.users.keys().cloned().collect(),
            groups: facts.accounts.groups.keys().cloned().collect(),
            services: facts.services.keys().cloned().collect(),
        }
    }

    /// The chosen config directory, or `default` when none was typed.
    fn location(&self, default: &Path) -> PathBuf {
        match self.location.trim() {
            "" => default.to_path_buf(),
            typed => PathBuf::from(typed),
        }
    }

    fn keep(&self) -> u32 {
        self.keep.trim().parse().unwrap_or(KEEP)
    }

    fn keep_days(&self) -> u32 {
        self.keep_days.trim().parse().unwrap_or(KEEP_DAYS)
    }

    /// Why the answers can't be used yet, if they can't.
    fn problem(&self) -> Option<String> {
        let location = self.location.trim();
        if !location.is_empty() && !Path::new(location).is_absolute() {
            return Some(format!("{location:?} isn't an absolute path"));
        }
        for (what, value) in [("generations to keep", &self.keep), ("days to keep", &self.keep_days)] {
            if !value.trim().is_empty() && value.trim().parse::<u32>().is_err() {
                return Some(format!("{what}: {value:?} isn't a number"));
            }
        }
        None
    }
}

/// The config being built, as the host module's top-level fields in the order they're written.
#[derive(Default)]
struct Host {
    fields: Vec<(String, Lua)>,
    packages: Vec<String>,
    copies: Vec<(PathBuf, PathBuf)>,
    home_user: Option<String>,
    home_dir: Option<PathBuf>,
}

impl Host {
    fn set(&mut self, key: &str, value: Lua) {
        self.fields.push((key.to_string(), value));
    }

    /// Adds packages to the host's `prefer` list, creating it if needed.
    fn prefer(&mut self, names: &[String]) {
        let mut all: Vec<String> = match self.fields.iter().find(|(key, _)| key == "prefer") {
            Some((_, Lua::List(items))) => items
                .iter()
                .filter_map(|item| match item {
                    Lua::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        all.extend(names.iter().cloned());
        all.sort();
        all.dedup();
        self.fields.retain(|(key, _)| key != "prefer");
        self.set("prefer", Lua::strings(&all));
    }
}

pub fn init(config_dir: &Path, host: &str, defaults: bool) -> Result<()> {
    require_root("init")?;
    let interactive = !defaults;
    if interactive && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!("`archix init` asks questions; run it in a terminal, or pass --defaults to take every default");
    }
    refuse_existing(config_dir)?;
    let paths = Paths::system();
    eprintln!("reading this system");
    let facts = gather(&paths, config_dir)?;
    let answers = Answers::recommended(&facts);
    let answers = if interactive {
        match tui::run(&facts, host, answers)? {
            Some(answers) => answers,
            None => bail!("init was cancelled; nothing was written"),
        }
    } else {
        Answers {
            dotfiles: BTreeSet::new(),
            ..answers
        }
    };
    if let Some(problem) = answers.problem() {
        bail!("{problem}");
    }
    if !interactive {
        println!(
            "\n  {}  {}",
            style("archix init").bold(),
            style("describe this machine as a config").dim()
        );
        report(&facts, &answers);
    }

    let chosen = answers.location(config_dir);
    let fresh = !chosen.exists();
    let result = write_and_lock(&facts, &answers, &paths, config_dir, &chosen, host, interactive);
    if result.is_err() && fresh {
        let _ = fs::remove_dir_all(&chosen);
        if chosen != config_dir && fs::symlink_metadata(config_dir).is_ok_and(|m| m.file_type().is_symlink()) {
            let _ = fs::remove_file(config_dir);
        }
    }
    result
}

/// Everything after the questions: writing the config, locking it and showing what the first switch would do.
fn write_and_lock(
    facts: &Facts,
    answers: &Answers,
    paths: &Paths,
    config_dir: &Path,
    chosen: &Path,
    host: &str,
    interactive: bool,
) -> Result<()> {
    let dir = settle_location(config_dir, chosen)?;
    let config = build(facts, answers, &dir, host);

    section("Lock");
    write_config(&dir, host, &config)?;
    let mut config = config;
    let evaluation = evaluate(&dir, host)?;
    let mut declared = declared(&evaluation.config)?;
    let mut lock = lock_as_installed(&declared, facts, paths)?;
    let swapped = swapped_providers(&lock, facts, &declared);
    let evaluation = if swapped.is_empty() {
        evaluation
    } else {
        note(format!(
            "preferring {}, so the lock keeps the providers this machine has",
            some_of(&swapped, 6)
        ));
        config.prefer(&swapped);
        write_host(&dir, host, &config)?;
        let evaluation = evaluate(&dir, host)?;
        declared = super::declared(&evaluation.config)?;
        lock = lock_as_installed(&declared, facts, paths)?;
        evaluation
    };
    let target = Target {
        config_dir: &dir,
        host,
        config: &evaluation.config,
    };
    lock.flatpak = facts
        .flatpak_apps
        .iter()
        .map(|(id, app)| {
            (
                id.clone(),
                LockedApp {
                    remote: app.origin.clone(),
                    reference: app.reference.clone(),
                    commit: app.commit.clone(),
                },
            )
        })
        .collect();
    let git_aur = aur_git(&declared)?;
    review(&git_aur, None, &lock, declared.aur_review && interactive, &confirm)?;
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
    if !lock.flatpak.is_empty() {
        done(format!(
            "{} locked to their installed commits",
            count(lock.flatpak.len(), "Flatpak app", "Flatpak apps")
        ));
    }
    if answers.git {
        run(Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]), "git init")?;
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "-A"]), "git add")?;
    }
    hand_over(&dir)?;

    section("Result");
    let (lines, warned, _) = pending(&target, &declared, &lock)?;
    if lines.is_empty() {
        done("the system matches the lock and the config: the first switch changes nothing");
    } else {
        warn("the first switch would still change these:");
        for line in &lines {
            println!("      {line}");
        }
    }
    print_warned(&warned);
    summary(&dir, host, &config, answers.git);
    Ok(())
}

/// The kernel package boot entries are built for: the running kernel's when a package owns it, as linux-cachyos does on CachyOS, else an installed kernel package.
fn boot_kernel(local: &BTreeMap<String, LocalPkg>, running: Option<&str>, installed: &[String]) -> String {
    let running = running.map(str::trim).filter(|base| local.contains_key(*base));
    let preferred = ["linux", "linux-lts", "linux-zen", "linux-hardened"]
        .into_iter()
        .find(|kernel| installed.iter().any(|base| base == kernel));
    running
        .or(preferred)
        .or(installed.first().map(String::as_str))
        .unwrap_or("linux")
        .to_string()
}

/// Reads everything init needs from the machine.
fn gather(paths: &Paths, default_dir: &Path) -> Result<Facts> {
    let machine = Machine {
        local: read_local_db(&paths.local_db())?,
        details: capture::read_installed(&paths.local_db())?,
        foreign: capture::pacman_query("m")?.into_iter().collect(),
    };
    let explicit = capture::pacman_query("en")?;
    let foreign_explicit = capture::pacman_query("em")?;
    let true_orphans: BTreeSet<String> = capture::pacman_query("dt")?.into_iter().collect();
    let all_orphans = capture::pacman_query("dtt")?;

    let conf = fs::read_to_string("/etc/pacman.conf").context("reading /etc/pacman.conf")?;
    let (official, custom) = capture::parse_pacman_conf(&conf, |path| fs::read_to_string(path).ok());

    let (mut unknown, mut aur_packages) = (Vec::new(), Vec::new());
    if !machine.foreign.is_empty() {
        let index = Index::load(
            &Archive::new(ARCHIVE_URL),
            archix::aur::DEFAULT_URL,
            &user_cache()?,
            false,
        )?;
        unknown = machine
            .foreign
            .iter()
            .filter(|name| index.get(name).is_none())
            .cloned()
            .collect();
        aur_packages = foreign_explicit
            .iter()
            .filter(|name| index.get(name).is_some())
            .cloned()
            .collect();
    }
    let mut packages: Vec<String> = explicit.iter().chain(&aur_packages).cloned().collect();
    packages.sort();
    packages.dedup();
    let (orphans, optional): (Vec<String>, Vec<String>) = all_orphans
        .into_iter()
        .filter(|name| !unknown.contains(name))
        .partition(|name| true_orphans.contains(name));

    let exe = std::env::current_exe()?;
    let mut skip: BTreeSet<String> = integration::files(paths, &exe)
        .into_iter()
        .chain(integration::boot_files(paths, &exe))
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    let (settings, settings_summary) = host_settings(&mut skip);
    let found = capture::config_files(&machine.details, &skip)?;

    let accounts = capture::parse_accounts(
        &fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?,
        &fs::read_to_string("/etc/group").context("reading /etc/group")?,
    );
    let dotfiles = accounts
        .users
        .iter()
        .map(|(name, user)| {
            let dir = PathBuf::from(user.home.clone().unwrap_or_else(|| format!("/home/{name}")));
            let (offered, secrets) = capture::dotfiles(&dir);
            (name.clone(), Home { dir, offered, secrets })
        })
        .collect();

    let boot = match capture::systemd_boot_esp() {
        None => BootFacts::NoLoader,
        Some(esp) => {
            let params = capture::kernel_params(&fs::read_to_string("/proc/cmdline").unwrap_or_default());
            if params.iter().any(|p| p.starts_with("root=")) {
                let running = fs::read_to_string(format!(
                    "/usr/lib/modules/{}/pkgbase",
                    fs::read_to_string("/proc/sys/kernel/osrelease")
                        .unwrap_or_default()
                        .trim()
                ))
                .ok();
                let installed: Vec<String> = archix::system::boot::installed_kernels()
                    .into_iter()
                    .map(|(base, _)| base)
                    .collect();
                let kernel = boot_kernel(&machine.local, running.as_deref(), &installed);
                let microcode = ["amd-ucode.img", "intel-ucode.img"]
                    .into_iter()
                    .filter(|image| Path::new(&esp).join(image).is_file())
                    .map(str::to_string)
                    .collect();
                BootFacts::Ready {
                    esp,
                    kernel,
                    microcode,
                    params,
                }
            } else {
                BootFacts::NoRoot { esp }
            }
        }
    };

    let mut flatpak_apps = flatpak::installed()?;
    let remotes = flatpak::remotes()?;
    let flatpak_gone: Vec<String> = flatpak_apps
        .iter()
        .filter(|(_, app)| app.origin != "flathub" && !remotes.contains_key(&app.origin))
        .map(|(id, _)| id.clone())
        .collect();
    flatpak_apps.retain(|id, _| !flatpak_gone.contains(id));
    let flatpak_remotes = remotes
        .into_iter()
        .filter(|(name, _)| name != "flathub" && flatpak_apps.values().any(|app| app.origin == *name))
        .collect();

    let owned: BTreeSet<String> = machine
        .details
        .values()
        .flat_map(|pkg| pkg.files.iter().map(|f| format!("/{f}")))
        .collect();
    let mut services = capture::changed_services()?;
    let unreproducible: Vec<String> = services
        .keys()
        .filter(|unit| {
            capture::unit_path(unit).is_some_and(|path| !path.starts_with("/etc/") && !owned.contains(&path))
        })
        .cloned()
        .collect();
    services.retain(|unit, _| !unreproducible.contains(unit));
    Ok(Facts {
        machine,
        default_dir: default_dir.to_path_buf(),
        git: which("git"),
        official,
        custom,
        packages,
        aur_packages,
        unknown,
        orphans,
        optional,
        settings,
        settings_summary,
        accounts,
        services,
        unreproducible,
        files: found.files,
        secrets: found.secrets.len(),
        btrfs: capture::is_btrfs(Path::new("/")),
        boot,
        flatpak_apps,
        flatpak_remotes,
        flatpak_gone,
        dotfiles,
    })
}

/// The config `answers` make of `facts`.
fn build(facts: &Facts, answers: &Answers, dir: &Path, host: &str) -> Host {
    let mut config = Host {
        packages: facts.packages.clone(),
        ..Host::default()
    };
    for (i, (key, value)) in facts.settings.iter().enumerate() {
        if answers.settings.contains(&i) {
            config.set(key, value.clone());
        }
    }
    if facts.official != ["core", "extra"] {
        config.set("repos", Lua::strings(&facts.official));
    }
    if !facts.custom.is_empty() {
        let repos = facts
            .custom
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
        if answers.fallback > 0 {
            config.set("third_party_fallback", Lua::str(FALLBACK[answers.fallback].0));
        }
    }
    if !answers.strict {
        config.set("undeclared_packages", Lua::str("warn"));
    }
    let mut ignored = facts.unknown.clone();
    if answers.keep_orphans {
        ignored.extend(facts.orphans.iter().cloned());
    }
    ignored.sort();
    ignored.dedup();
    if !ignored.is_empty() {
        config.set("ignored_packages", Lua::strings(&ignored));
    }
    if !facts.optional.is_empty() {
        config.prefer(&facts.optional);
    }
    if !facts.aur_packages.is_empty() {
        if !answers.aur_review {
            config.set("aur", Lua::Table(vec![("review".into(), Lua::Bool(false))]));
        }
        if answers.missing_versions > 0 {
            config.set(
                "missing_versions",
                Lua::str(MISSING_VERSIONS[answers.missing_versions].0),
            );
        }
    }
    if !facts.flatpak_apps.is_empty() {
        let ids: Vec<String> = facts
            .flatpak_apps
            .iter()
            .map(|(id, app)| {
                if app.origin == "flathub" {
                    id.clone()
                } else {
                    format!("{}:{id}", app.origin)
                }
            })
            .collect();
        let mut fields = vec![("apps".to_string(), Lua::strings(&ids))];
        if !facts.flatpak_remotes.is_empty() {
            let mut table: Vec<(String, Lua)> = vec![(
                "flathub".to_string(),
                Lua::Table(vec![(
                    "url".into(),
                    Lua::str("https://dl.flathub.org/repo/flathub.flatpakrepo"),
                )]),
            )];
            for (name, remote) in &facts.flatpak_remotes {
                let mut fields = vec![("url".to_string(), Lua::str(&remote.url))];
                if !remote.gpg_verify {
                    fields.push(("gpg_verify".into(), Lua::Bool(false)));
                }
                table.push((name.clone(), Lua::Table(fields)));
            }
            fields.push(("remotes".into(), Lua::Table(table)));
        }
        config.set("flatpak", Lua::Table(fields));
    }
    if !answers.users.is_empty() {
        let users = facts
            .accounts
            .users
            .iter()
            .filter(|(name, _)| answers.users.contains(*name))
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
    if !answers.groups.is_empty() {
        let groups = facts
            .accounts
            .groups
            .iter()
            .filter(|(name, _)| answers.groups.contains(*name))
            .map(|(name, gid)| (name.clone(), Lua::Table(vec![("gid".into(), Lua::Int((*gid).into()))])))
            .collect();
        config.set("groups", Lua::Table(groups));
    }
    if !answers.services.is_empty() {
        config.set(
            "services",
            Lua::Table(
                facts
                    .services
                    .iter()
                    .filter(|(unit, _)| answers.services.contains(*unit))
                    .map(|(unit, on)| (unit.clone(), Lua::Bool(*on)))
                    .collect(),
            ),
        );
    }
    let mut table = Vec::new();
    for i in &answers.files {
        let file = &facts.files[*i];
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
    let mut boot = Vec::new();
    if let BootFacts::Ready {
        esp,
        kernel,
        microcode,
        params,
    } = &facts.boot
        && answers.boot
    {
        boot.push(("loader".to_string(), Lua::str("systemd-boot")));
        if esp != "/boot" {
            boot.push(("esp".into(), Lua::str(esp)));
        }
        if kernel != "linux" {
            boot.push(("kernel".into(), Lua::str(kernel)));
        }
        if !microcode.is_empty() {
            boot.push(("microcode".into(), Lua::strings(microcode)));
        }
        config.set("kernel_params", Lua::strings(params));
    }
    if facts.btrfs && !answers.snapshots {
        boot.push(("snapshots".to_string(), Lua::Bool(false)));
    }
    if !boot.is_empty() {
        config.set("boot", Lua::Table(boot));
    }
    let mut generations = Vec::new();
    if answers.keep() != KEEP {
        generations.push(("keep".to_string(), Lua::Int(answers.keep().into())));
    }
    if answers.keep_days() != KEEP_DAYS {
        generations.push(("keep_days".to_string(), Lua::Int(answers.keep_days().into())));
    }
    if !generations.is_empty() {
        config.set("generations", Lua::Table(generations));
    }
    if answers.read_only {
        config.set("mutable_files", Lua::Bool(false));
    }
    if let Some((user, Home { dir: home, offered, .. })) = facts.dotfiles.iter().nth(answers.dotfiles_user)
        && !answers.dotfiles.is_empty()
    {
        let mut files = Vec::new();
        for relative in &answers.dotfiles {
            let source = format!("home/{relative}");
            config.copies.push((home.join(relative), dir.join(&source)));
            let replaced = offered.iter().any(|d| d.name == *relative && d.replaced);
            let mut fields = vec![("source".to_string(), Lua::str(&source))];
            // Directories stay live; a file its app replaces on save gets a writable copy, since a link wouldn't survive.
            if home.join(relative).is_dir() || (!replaced && !answers.read_only) {
                fields.push(("live".into(), Lua::Bool(true)));
            } else if replaced {
                fields.push(("mutable".into(), Lua::Bool(true)));
            }
            files.push((relative.clone(), Lua::Table(fields)));
        }
        config.set(
            "home",
            Lua::Table(vec![
                ("user".into(), Lua::str(user)),
                ("files".into(), Lua::Table(files)),
            ]),
        );
        config.home_user = Some(user.clone());
        config.home_dir = Some(home.clone());
    }
    config
}

/// What `--defaults` captured, in the same sections the interactive steps use.
fn report(facts: &Facts, answers: &Answers) {
    section("Packages");
    done(format!("repos {}", facts.official.join(", ")));
    if !facts.custom.is_empty() {
        let names: Vec<String> = facts.custom.iter().map(|(name, _)| name.clone()).collect();
        done(format!("third-party repos {}", names.join(", ")));
    }
    let from_aur = if facts.aur_packages.is_empty() {
        String::new()
    } else {
        format!(", {} from the AUR", facts.aur_packages.len())
    };
    done(format!(
        "{} declared{from_aur}",
        count(facts.packages.len(), "package", "packages")
    ));
    if !facts.unknown.is_empty() {
        warn(format!(
            "not in any repo or the AUR, so archix leaves them alone: {}",
            some_of(&facts.unknown, 8)
        ));
    }
    if !facts.orphans.is_empty() {
        note(format!(
            "{} that nothing needs any more, kept in ignored_packages: {}",
            count(facts.orphans.len(), "dependency", "dependencies"),
            some_of(&facts.orphans, 8)
        ));
    }
    if !facts.flatpak_apps.is_empty() {
        done(count(facts.flatpak_apps.len(), "Flatpak app", "Flatpak apps"));
    }
    if !facts.flatpak_gone.is_empty() {
        warn(format!(
            "their remote is gone, so archix leaves these Flatpak apps alone: {}",
            some_of(&facts.flatpak_gone, 6)
        ));
    }
    section("System");
    if !facts.settings_summary.is_empty() {
        done(facts.settings_summary.join(", "));
    }
    let users: Vec<String> = facts.accounts.users.keys().cloned().collect();
    if !users.is_empty() {
        done(format!(
            "{}: {}",
            count(users.len(), "user", "users"),
            some_of(&users, 6)
        ));
    }
    if !facts.unreproducible.is_empty() {
        warn(format!(
            "left out, since no package or /etc file provides their unit: {}",
            some_of(&facts.unreproducible, 6)
        ));
    }
    if !facts.services.is_empty() {
        done(format!(
            "{} that differ from their preset",
            count(facts.services.len(), "service", "services")
        ));
    }
    section("Config files");
    if facts.secrets > 0 {
        note(secret_note(facts.secrets, "file"));
    }
    for i in &answers.files {
        done(&facts.files[*i].path);
    }
    if answers.files.is_empty() {
        note("none");
    }
    let left = facts.files.len() - answers.files.len();
    if left > 0 {
        note(format!(
            "{} left unmanaged; the wizard (`archix init` without --defaults) lets you pick them",
            count(left, "more file", "more files")
        ));
    }
    section("Boot");
    match &facts.boot {
        BootFacts::NoLoader => note("no systemd-boot found, so archix leaves booting alone"),
        BootFacts::NoRoot { .. } => warn("the kernel command line has no root=, so archix leaves booting alone"),
        BootFacts::Ready { esp, kernel, .. } => done(format!("boot entries on {esp}, kernel {kernel}")),
    }
    if facts.btrfs {
        done("a snapshot of / before every switch");
    }
    section("Generations");
    done(format!(
        "gc keeps the newest {} generations and any younger than {} days",
        answers.keep(),
        answers.keep_days()
    ));
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

/// Creates the chosen directory; choosing one other than `default` leaves `default` as a symlink to it.
fn settle_location(default: &Path, chosen: &Path) -> Result<PathBuf> {
    fs::create_dir_all(chosen).with_context(|| format!("creating {}", chosen.display()))?;
    if chosen != default {
        refuse_existing(chosen)?;
        if fs::symlink_metadata(default).is_ok() {
            bail!(
                "{} already exists, so it can't link to {}; move it away or keep the config there",
                default.display(),
                chosen.display()
            );
        }
        symlink(chosen, default).with_context(|| format!("linking {} to {}", default.display(), chosen.display()))?;
        done(format!("{} links to {}", default.display(), chosen.display()));
    }
    Ok(chosen.to_path_buf())
}

fn exact(path: &str, content: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text == content)
}

/// Hostname, timezone, locales and keymap, but only where the option would write exactly what's there now.
fn host_settings(skip: &mut BTreeSet<String>) -> (Vec<(String, Lua)>, Vec<String>) {
    let (mut fields, mut summary) = (Vec::new(), Vec::new());
    let hostname = fs::read_to_string("/etc/hostname").unwrap_or_default();
    let hostname = hostname.trim();
    if !hostname.is_empty() && exact("/etc/hostname", &format!("{hostname}\n")) {
        fields.push(("hostname".to_string(), Lua::str(hostname)));
        skip.insert("/etc/hostname".into());
        summary.push(format!("hostname {hostname}"));
    }
    if let Ok(target) = fs::read_link("/etc/localtime")
        && let Some(zone) = target.to_string_lossy().strip_prefix("/usr/share/zoneinfo/")
    {
        fields.push(("timezone".to_string(), Lua::str(zone)));
        summary.push(format!("timezone {zone}"));
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
        fields.push(("locales".to_string(), Lua::strings(&locales)));
        skip.extend(["/etc/locale.gen".to_string(), "/etc/locale.conf".to_string()]);
        summary.push(format!("locales {}", locales.join(" ")));
    }
    let vconsole = fs::read_to_string("/etc/vconsole.conf").unwrap_or_default();
    if let Some(keymap) = vconsole.strip_prefix("KEYMAP=").map(str::trim)
        && vconsole == format!("KEYMAP={keymap}\n")
    {
        fields.push(("keymap".to_string(), Lua::str(keymap)));
        skip.insert("/etc/vconsole.conf".into());
        summary.push(format!("keymap {keymap}"));
    }
    (fields, summary)
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
    write_host(dir, host, config)?;
    write_installed(dir, host, &config.packages)?;
    for (from, to) in &config.copies {
        copy_tree(from, to)?;
    }
    if let Some(home) = &config.home_dir {
        own_tree(&dir.join("home"), home)?;
    }
    Ok(())
}

fn write_host(dir: &Path, host: &str, config: &Host) -> Result<()> {
    let body = Lua::Table(config.fields.clone()).render(0);
    let path = installed::default_dir(dir, host).join("init.lua");
    fs::write(
        &path,
        format!("-- Written by `archix init` from this machine; edit it freely.\nreturn {body}\n"),
    )
    .with_context(|| format!("writing {}", path.display()))
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
        ..Default::default()
    })
    .map_err(|errors| anyhow::anyhow!("the generated config doesn't evaluate; please report this:\n{errors}"))
}

/// Locks the snapshot the installed official packages were built for, then pins every version that differs from it.
fn lock_as_installed(declared: &Declared, facts: &Facts, paths: &Paths) -> Result<HostLock> {
    let machine = &facts.machine;
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
    let recipes = Recipes::none();
    let aur = aur_source(&git, declared, &archive, None, false, &recipes);
    fit(
        &archive,
        &user_cache()?,
        paths,
        &declared.packages,
        &declared.prefer,
        &Candidates { dates, day },
        None,
        &machine.local,
        &declared.repos,
        Some(&aur),
        Fit::Exact,
    )
}

/// Installed dependencies the lock would replace with another provider of the same thing.
fn swapped_providers(lock: &HostLock, facts: &Facts, declared: &Declared) -> Vec<String> {
    plan(&lock.packages, &facts.machine.local)
        .into_iter()
        .filter_map(|change| match change {
            Change::Remove { name, .. } if !declared.ignored.contains(&name) && !facts.orphans.contains(&name) => {
                Some(name)
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(names: &[&str]) -> BTreeMap<String, LocalPkg> {
        names
            .iter()
            .map(|name| {
                let pkg = LocalPkg {
                    name: name.to_string(),
                    version: "1-1".into(),
                    arch: "x86_64".into(),
                    explicit: true,
                    installed_at: None,
                };
                (name.to_string(), pkg)
            })
            .collect()
    }

    #[test]
    fn boot_entries_are_for_the_kernel_the_machine_runs() {
        let cachyos = machine(&["linux-cachyos", "linux-cachyos-lts"]);
        let installed = ["linux-cachyos".to_string(), "linux-cachyos-lts".to_string()];
        assert_eq!(
            boot_kernel(&cachyos, Some("linux-cachyos-lts\n"), &installed),
            "linux-cachyos-lts"
        );
        assert_eq!(
            boot_kernel(&cachyos, None, &installed),
            "linux-cachyos",
            "without the running one, an installed kernel"
        );
        let arch = machine(&["linux", "linux-lts"]);
        let installed = ["linux".to_string(), "linux-lts".to_string()];
        assert_eq!(boot_kernel(&arch, Some("linux"), &installed), "linux");
    }
}
