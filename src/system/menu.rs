//! The boot menu archix keeps: an entry for the current system, one per snapshot worth booting and the fallback slot, each starting a shared boot image, and the state that maps every entry to what it boots.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::boot::{self, BootPlan, Loader};
use crate::paths::Paths;
use crate::snapshot::{Kind, Record};

/// The current system's entry, which boots `@` itself.
pub const CURRENT: &str = "archix-current.conf";
const CURRENT_STEM: &str = "archix-current";
const SNAPSHOT_STEM: &str = "archix-snapshot-";
/// loader.conf's `preferred`: systemd-boot starts the first entry in sort-key order that matches and still has tries.
pub const PREFERRED: &str = "archix-*";
/// Where the shared boot images live on the ESP, outside EFI/Linux so systemd-boot lists them only through entries.
pub const IMAGES: &str = "EFI/archix";
const STATE: &str = "state.json";
pub const FORMAT: u32 = 1;
/// Tries a snapshot's entry gets, so a fallback that fails too passes on to the next snapshot.
const SNAPSHOT_TRIES: u32 = 1;

/// What an archix entry boots.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Entry {
    Current,
    /// Another image of the current system: the protected twin or another installed kernel, by its suffix.
    Extra(String),
    Snapshot(u32),
}

impl Entry {
    pub fn id(&self) -> String {
        match self {
            Entry::Current => CURRENT.to_string(),
            Entry::Extra(suffix) => format!("{CURRENT_STEM}-{suffix}.conf"),
            Entry::Snapshot(number) => format!("{SNAPSHOT_STEM}{number}.conf"),
        }
    }

    /// The entry an id (an entry file's name without its boot counters) names; None for anything archix doesn't write here, the rescue entry included.
    pub fn parse(id: &str) -> Option<Entry> {
        let stem = boot::entry_id(id);
        let stem = stem.strip_suffix(".conf")?;
        if stem == CURRENT_STEM {
            return Some(Entry::Current);
        }
        if let Some(number) = stem.strip_prefix(SNAPSHOT_STEM) {
            if number.is_empty() || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            return number.parse().ok().map(Entry::Snapshot);
        }
        let suffix = stem.strip_prefix(CURRENT_STEM)?.strip_prefix('-')?;
        (!suffix.is_empty()).then(|| Entry::Extra(suffix.to_string()))
    }
}

/// One boot image on the ESP and what it was built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// Its file name under EFI/archix, which carries a hash of everything in it.
    pub name: String,
    /// The kernel release it starts.
    pub version: String,
}

/// The images of the current system: the main one, then each extra one with the suffix and title of its entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Images {
    pub main: Option<Image>,
    #[serde(default)]
    pub extras: Vec<(String, String, Image)>,
}

/// What archix keeps about the menu, in @archix where every snapshot's boot and the initramfs can read it, else in the root's archix state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub format: u32,
    #[serde(default)]
    pub current: Images,
    /// The main image of the system each snapshot holds, recorded when it was taken.
    #[serde(default)]
    pub snapshots: BTreeMap<u32, String>,
    /// The fallback slot: the first snapshot taken since the last boot that counted as good.
    #[serde(default)]
    pub slot: Option<u32>,
    /// A boot counted as good since the slot was set, so the next snapshot takes it.
    #[serde(default)]
    pub armed: bool,
    /// Snapshots whose entries used up their tries, which the missing-snapshot guard never boots.
    #[serde(default)]
    pub failed: BTreeSet<u32>,
    /// The units that had failed at the last boot that counted as good, which the health check allows.
    #[serde(default)]
    pub blessed_failed_units: Vec<String>,
}

/// Where the menu's state and entry records are: @archix's boot directory when `root`/.archix is a subvolume of its own, else the archix state's.
pub fn dir(paths: &Paths) -> PathBuf {
    match crate::snapshot::archix_on(&paths.root) {
        true => paths.root.join(crate::snapshot::MOUNT).join("boot"),
        false => paths.state.join("boot"),
    }
}

/// Moves the menu's state from the archix state into @archix the first time @archix is mounted, as the switch that set rollbacks up wrote it before then.
pub fn adopt(paths: &Paths) -> Result<()> {
    let (to, from) = (dir(paths), paths.state.join("boot"));
    if to == from || to.join(STATE).exists() || !from.join(STATE).exists() {
        return Ok(());
    }
    for sub in ["entries", "cmdlines"] {
        for file in fs::read_dir(from.join(sub)).into_iter().flatten().flatten() {
            crate::durable::copy(&file.path(), &to.join(sub).join(file.file_name()))?;
        }
    }
    crate::durable::copy(&from.join(STATE), &to.join(STATE))
}

/// The record of the entry `id`, written before the entry itself: the initramfs boots only entries archix recorded, as the ESP is unsigned.
pub fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("entries").join(boot::entry_id(id))
}

pub fn load(dir: &Path) -> Result<State> {
    let path = dir.join(STATE);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(dir: &Path, state: &State) -> Result<()> {
    let state = State {
        format: FORMAT,
        ..state.clone()
    };
    crate::durable::write(&dir.join(STATE), serde_json::to_string_pretty(&state)?.as_bytes())
}

/// Called once snapshot `number` exists: it boots the image the current system boots, and takes the slot if it's free or a good boot came since the slot was set.
pub fn snapshot_taken(paths: &Paths, number: u32) -> Result<()> {
    let dir = dir(paths);
    let mut state = load(&dir)?;
    taken(&mut state, number);
    save(&dir, &state)
}

fn taken(state: &mut State, number: u32) {
    if let Some(image) = &state.current.main {
        state.snapshots.insert(number, image.name.clone());
    }
    if state.slot.is_none() || state.armed {
        state.slot = Some(number);
        state.armed = false;
    }
}

/// Forgets snapshots that are gone, the slot included.
pub fn forget_missing(state: &mut State, existing: &BTreeSet<u32>) {
    state.snapshots.retain(|number, _| existing.contains(number));
    state.failed.retain(|number| existing.contains(number));
    if state.slot.is_some_and(|slot| !existing.contains(&slot)) {
        state.slot = None;
    }
}

/// One entry the menu should have.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub entry: Entry,
    pub image: String,
    pub title: String,
    pub sort_key: String,
    pub version: String,
}

/// The sort keys: the current system first, then the slot, then the other snapshots newest first (by version), then the current system's extra images; the rescue entry's comes after them all.
pub const SORT_CURRENT: &str = "archix-0";
pub const SORT_SLOT: &str = "archix-1";
pub const SORT_SNAPSHOT: &str = "archix-2";
pub const SORT_EXTRA: &str = "archix-3";

/// Which snapshots get an entry: those switches took, roots a keep replaced, and those pacman took before a transaction that changed the boot image, the newest `cap` of them, plus the slot; each needs an image on record.
pub fn with_entries(state: &State, records: &[Record], cap: u32) -> Vec<u32> {
    let image = |number: u32| state.snapshots.get(&number);
    let mut candidates = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let Some(own) = image(record.number) else { continue };
        let wanted = match record.kind {
            Kind::Switch | Kind::Replaced => true,
            Kind::Pacman => {
                let next = records[i + 1..].iter().find_map(|later| image(later.number)).or(state
                    .current
                    .main
                    .as_ref()
                    .map(|image| &image.name));
                next != Some(own)
            }
            _ => false,
        };
        if wanted {
            candidates.push(record.number);
        }
    }
    let mut chosen: Vec<u32> = candidates.iter().rev().take(cap as usize).copied().collect();
    if let Some(slot) = state.slot
        && image(slot).is_some()
        && records.iter().any(|record| record.number == slot)
        && !chosen.contains(&slot)
    {
        chosen.push(slot);
    }
    chosen.sort_unstable();
    chosen
}

/// The entries the menu should have, in the order systemd-boot sorts them.
pub fn plan(state: &State, records: &[Record], cap: u32) -> Vec<Planned> {
    let mut planned = Vec::new();
    if let Some(main) = &state.current.main {
        planned.push(Planned {
            entry: Entry::Current,
            image: main.name.clone(),
            title: "Arch Linux (archix)".to_string(),
            sort_key: SORT_CURRENT.to_string(),
            version: main.version.clone(),
        });
    }
    let mut snapshots: Vec<u32> = with_entries(state, records, cap);
    snapshots.reverse();
    for number in snapshots {
        let record = records.iter().find(|record| record.number == number);
        let state_line = record.map(snapshot_title).unwrap_or_default();
        planned.push(Planned {
            entry: Entry::Snapshot(number),
            image: state.snapshots[&number].clone(),
            title: format!("archix snapshot {number}, read-only: {state_line}"),
            sort_key: match state.slot == Some(number) {
                true => SORT_SLOT.to_string(),
                false => SORT_SNAPSHOT.to_string(),
            },
            version: number.to_string(),
        });
    }
    for (suffix, title, image) in &state.current.extras {
        planned.push(Planned {
            entry: Entry::Extra(suffix.clone()),
            image: image.name.clone(),
            title: title.clone(),
            sort_key: SORT_EXTRA.to_string(),
            version: image.version.clone(),
        });
    }
    planned
}

/// What a snapshot's entry title says about the system it holds.
fn snapshot_title(record: &Record) -> String {
    let date = crate::date::Date::from_unix(record.created);
    let state = match (&record.tree, record.generation) {
        (_, Some(generation)) => format!("generation {generation}"),
        (Some(tree), None) => format!("tree {}", &tree[..tree.len().min(12)]),
        (None, None) => "before archix's first switch".to_string(),
    };
    match record.label.is_empty() {
        true => format!("{date}, {state}"),
        false => format!("{date}, {state}, {}", record.label),
    }
}

/// An entry's text; `uki` when systemd-boot counts its tries, which it does only for `linux` and `uki` entries.
pub fn text(planned: &Planned, counted: bool) -> String {
    let key = if counted { "uki" } else { "efi" };
    format!(
        "title {}\nversion {}\nsort-key {}\n{key} /{IMAGES}/{}\n",
        planned.title, planned.version, planned.sort_key, planned.image
    )
}

/// archix's entry files on the ESP by id.
pub fn on_esp(esp: &Path) -> BTreeMap<String, String> {
    entry_files(esp)
        .into_iter()
        .map(|name| (boot::entry_id(&name), name))
        .collect()
}

/// Whether the entry file `name` on the ESP is whole: a title and a uki or efi line whose image is there, as fsck.fat can leave one empty or holding NULs after a cut.
fn whole_entry(esp: &Path, name: &str) -> bool {
    let Ok(text) = fs::read_to_string(esp.join("loader/entries").join(name)) else {
        return false;
    };
    let title = text.lines().any(|line| line.starts_with("title "));
    let image = text.lines().find_map(|line| {
        let (key, path) = line.split_once(' ')?;
        matches!(key, "uki" | "efi").then(|| path.trim().trim_start_matches('/'))
    });
    title && !text.contains('\0') && image.is_some_and(|image| esp.join(image).is_file())
}

/// The snapshots the menu gives an entry whose file on the ESP is missing or not whole, for `archix status`; the next refresh writes them again.
pub fn missing_entries(esp: &Path, state: &State, records: &[Record], cap: u32) -> Vec<u32> {
    let on = on_esp(esp);
    plan(state, records, cap)
        .iter()
        .filter_map(|planned| match planned.entry {
            Entry::Snapshot(number) => {
                let id = planned.entry.id();
                (!on.get(&id).is_some_and(|name| whole_entry(esp, name))).then_some(number)
            }
            _ => None,
        })
        .collect::<BTreeSet<u32>>()
        .into_iter()
        .collect()
}

/// Drops the other names of an entry on the ESP under two, as a cut between naming its new file and removing its old one leaves it: of the whole ones, `booted` (the file this boot's try is counted on) stays if it's one, else a counted one, else any; with none whole they all stay for the next refresh to rewrite. Returns the names dropped. systemd-bless-boot can't rename a counted file onto a name that's there.
pub fn drop_second_names(esp: &Path, booted: Option<&str>) -> Result<Vec<String>> {
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in entry_files(esp) {
        by_id.entry(boot::entry_id(&name)).or_default().push(name);
    }
    let mut dropped = Vec::new();
    for (id, mut names) in by_id {
        if names.len() < 2 {
            continue;
        }
        names.sort();
        let whole: Vec<&String> = names.iter().filter(|name| whole_entry(esp, name)).collect();
        let Some(keep) = booted
            .and_then(|booted| whole.iter().find(|name| ***name == booted))
            .or_else(|| whole.iter().find(|name| name.contains('+')))
            .or_else(|| whole.first())
            .map(|name| name.to_string())
        else {
            eprintln!(
                "warning: no file of the boot entry {id} on the ESP is whole ({}); the next refresh writes it again",
                names.join(", ")
            );
            continue;
        };
        for name in names.into_iter().filter(|name| *name != keep) {
            let path = esp.join("loader/entries").join(&name);
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            dropped.push(name);
        }
    }
    if !dropped.is_empty() {
        crate::durable::sync_esp(esp)?;
    }
    Ok(dropped)
}

/// The names of archix's entry files on the ESP, two for one entry included.
fn entry_files(esp: &Path) -> Vec<String> {
    fs::read_dir(esp.join("loader/entries"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|file| file.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".conf") && Entry::parse(name).is_some())
        .collect()
}

/// The name an entry's file gets: an entry already there keeps its boot counters unless the current system's are reset after a change; a new counted one starts with its tries.
fn file_name(planned: &Planned, existing: Option<&String>, counted: bool, tries: u32, reset: bool) -> String {
    let id = planned.entry.id();
    let stem = id.trim_end_matches(".conf");
    let fresh = |tries: u32| match counted && tries > 0 {
        true => format!("{stem}+{tries}.conf"),
        false => id.clone(),
    };
    match (&planned.entry, existing) {
        (Entry::Current, _) if reset => fresh(tries),
        (Entry::Extra(_), _) => id.clone(),
        (_, Some(name)) if counted || !name.contains('+') => name.clone(),
        (Entry::Current, _) => fresh(tries),
        (Entry::Snapshot(_), _) => fresh(SNAPSHOT_TRIES),
    }
}

/// The other spelling of a counted entry name that hasn't been tried, +N and +N-0 (N left, none done), which systemd-boot and systemd-bless-boot read alike: a changed entry is written under it, so no file is ever renamed over another on the ESP; an entry with tries done has none.
fn alternate(name: &str) -> Option<String> {
    let (stem, counters) = name.strip_suffix(".conf")?.split_once('+')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    match counters.strip_suffix("-0") {
        Some(left) if digits(left) => Some(format!("{stem}+{left}.conf")),
        None if digits(counters) => Some(format!("{stem}+{counters}-0.conf")),
        _ => None,
    }
}

/// How `write` writes the menu.
pub struct Write<'a> {
    pub esp: &'a Path,
    pub dir: &'a Path,
    pub counted: bool,
    pub tries: u32,
    /// Give the current system's entry its tries again, after a change.
    pub reset: bool,
    pub signed: bool,
}

/// Takes archix's entries `planned` doesn't have off the ESP, with their records.
fn drop_unwanted(esp: &Path, dir: &Path, planned: &[Planned]) -> Result<()> {
    let wanted: BTreeSet<String> = planned.iter().map(|p| p.entry.id()).collect();
    let mut dropped = false;
    for name in entry_files(esp) {
        let id = boot::entry_id(&name);
        if !wanted.contains(&id) {
            let path = esp.join("loader/entries").join(&name);
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            let _ = fs::remove_file(record_path(dir, &id));
            dropped = true;
        }
    }
    match dropped {
        true => crate::durable::sync_esp(esp),
        false => Ok(()),
    }
}

/// Removes the images neither `planned` nor an entry still on the ESP starts; FAT reuses freed clusters, so an image goes only once no entry on the disk names it.
fn remove_unused_images(esp: &Path, planned: &[Planned]) -> Result<()> {
    let mut used: BTreeSet<String> = planned.iter().map(|p| p.image.clone()).collect();
    for name in on_esp(esp).values() {
        let text = fs::read_to_string(esp.join("loader/entries").join(name)).unwrap_or_default();
        if let Some(image) = boot::recorded_uki(&text) {
            used.insert(image.rsplit('/').next().unwrap_or_default().to_string());
        }
    }
    let mut removed = false;
    for file in fs::read_dir(esp.join(IMAGES)).into_iter().flatten().flatten() {
        let name = file.file_name().to_string_lossy().into_owned();
        if name.ends_with(".efi") && !used.contains(&name) {
            fs::remove_file(file.path()).with_context(|| format!("removing {}", file.path().display()))?;
            removed = true;
        }
    }
    match removed {
        true => crate::durable::sync_esp(esp),
        false => Ok(()),
    }
}

/// Puts the planned entries on the ESP, each recorded first, once archix's other entries are gone; then drops the images no entry starts and points loader.conf at the menu.
pub fn write(how: &Write, planned: &[Planned]) -> Result<()> {
    let esp = how.esp;
    let entries = esp.join("loader/entries");
    fs::create_dir_all(&entries).with_context(|| format!("creating {}", entries.display()))?;
    super::esp::clean_esp(esp)?;
    drop_unwanted(esp, how.dir, planned)?;
    let existing = on_esp(esp);
    let names = entry_files(esp);
    // (old name if this one replaces it, new name, text); a name is never reused for different text, so no file is renamed over another.
    let mut changed: Vec<(Option<String>, String, String)> = Vec::new();
    let mut in_place = Vec::new();
    for planned in planned {
        let id = planned.entry.id();
        let counted = how.counted && !matches!(planned.entry, Entry::Extra(_));
        let text = text(planned, counted);
        let mut name = file_name(planned, existing.get(&id), counted, how.tries, how.reset);
        crate::durable::write(&record_path(how.dir, &id), text.as_bytes())?;
        let old = existing.get(&id).cloned();
        if old.as_deref() == Some(name.as_str()) && fs::read_to_string(entries.join(&name)).is_ok_and(|now| now == text)
        {
            continue;
        }
        if old.as_deref() == Some(name.as_str()) {
            let stem = id.trim_end_matches(".conf");
            let tries = if matches!(planned.entry, Entry::Current) {
                how.tries
            } else {
                SNAPSHOT_TRIES
            };
            name = match alternate(&name) {
                Some(other) => other,
                None if counted && tries > 0 => format!("{stem}+{tries}.conf"),
                None => {
                    in_place.push(name.clone());
                    name
                }
            };
        }
        changed.push((old, name, text));
    }
    // The data is on the disk before a directory entry gives it a size, as a cut can otherwise leave the entry naming NULs.
    for (_, name, text) in &changed {
        let tmp = entries.join(format!("{name}.tmp"));
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    }
    if !changed.is_empty() {
        crate::durable::sync_esp(esp)?;
    }
    // Only with counting off has an entry no second name; its old file goes first, leaving that entry briefly unbootable rather than renamed over.
    for name in &in_place {
        fs::remove_file(entries.join(name)).with_context(|| format!("removing {name}"))?;
    }
    if !in_place.is_empty() {
        crate::durable::sync_esp(esp)?;
    }
    for (_, name, _) in &changed {
        crate::durable::rename_esp(&entries.join(format!("{name}.tmp")), &entries.join(name))?;
        crate::testhooks::mark("menu-entry-written");
    }
    // The new names reach the disk before the old ones go, so a cut leaves an entry under two names, which drop_second_names settles, never under none.
    crate::durable::sync_esp(esp)?;
    let kept: BTreeSet<&String> = changed.iter().map(|(_, name, _)| name).collect();
    let ids: BTreeSet<String> = planned.iter().map(|p| p.entry.id()).collect();
    for old in names
        .iter()
        .filter(|old| ids.contains(&boot::entry_id(old)) && !kept.contains(old))
    {
        let id = boot::entry_id(old);
        if changed.iter().any(|(_, name, _)| boot::entry_id(name) == id) || existing.get(&id) != Some(old) {
            fs::remove_file(entries.join(old)).with_context(|| format!("removing {old}"))?;
            crate::testhooks::mark("menu-entry-replaced");
        }
    }
    crate::durable::sync_esp(esp)?;
    crate::testhooks::mark("menu-synced");
    remove_unused_images(esp, planned)?;
    if planned.is_empty() {
        return Ok(());
    }
    boot::point_loader(esp, PREFERRED, CURRENT, how.signed, Some(&loader_copy(how.dir)))
}

/// Takes the snapshot entries whose image is neither on the ESP nor in `built` out of `planned`, as after a cut between recording a new image and copying it, when a try boot or keep refreshes the menu from another root; returns them. The current system's and the fallback slot's entries stay: the slot's missing image fails the refresh here, naming it, as a menu without it would leave a broken boot nowhere to fall back to.
fn drop_unreachable(
    esp: &Path,
    built: &Path,
    planned: &mut Vec<Planned>,
    slot: Option<u32>,
) -> Result<Vec<(u32, String)>> {
    let reachable = |image: &str| esp.join(IMAGES).join(image).exists() || built.join(image).is_file();
    // With no current entry on the ESP (wiped, as before a live-USB recovery's keep), keeping the menu as it is would leave nothing to boot.
    let menu_left = on_esp(esp).keys().any(|id| Entry::parse(id) == Some(Entry::Current));
    if let Some(lost) = planned
        .iter()
        .find(|p| slot.is_some_and(|slot| p.entry == Entry::Snapshot(slot)) && !reachable(&p.image))
        && menu_left
    {
        bail!(
            "the fallback slot, snapshot {}, boots the image {}, which is neither on the ESP nor among the images this root built, so the menu isn't written rather than written without the slot",
            slot.unwrap_or_default(),
            lost.image
        );
    }
    let mut dropped = Vec::new();
    planned.retain(|p| match (&p.entry, reachable(&p.image)) {
        (Entry::Snapshot(number), false) => {
            dropped.push((*number, p.image.clone()));
            false
        }
        _ => true,
    });
    Ok(dropped)
}

/// The images `planned` needs that aren't on the ESP yet, by name.
fn missing_images<'a>(esp: &Path, planned: &'a [Planned]) -> BTreeSet<&'a str> {
    planned
        .iter()
        .map(|p| p.image.as_str())
        .filter(|name| !esp.join(IMAGES).join(name).exists())
        .collect()
}

/// Makes room for the images that aren't on the ESP yet, with a margin: drops archix's entries `planned` doesn't have and the images nothing starts, then the oldest snapshot entries (never the slot's or the current system's) until they fit; `size` gives an image's size, `free` the room left. Fails naming what's short when even that isn't enough; returns the snapshots whose entries went.
pub fn fit(
    esp: &Path,
    dir: &Path,
    planned: &mut Vec<Planned>,
    size: &dyn Fn(&str) -> u64,
    free: &dyn Fn() -> Result<u64>,
) -> Result<Vec<u32>> {
    let mut dropped = Vec::new();
    loop {
        drop_unwanted(esp, dir, planned)?;
        remove_unused_images(esp, planned)?;
        let needed: u64 = missing_images(esp, planned).iter().map(|name| size(name)).sum::<u64>() + boot::MARGIN;
        let free = free()?;
        if free >= needed {
            return Ok(dropped);
        }
        let oldest = planned
            .iter()
            .filter(|p| p.sort_key == SORT_SNAPSHOT)
            .filter_map(|p| match p.entry {
                Entry::Snapshot(number) => Some(number),
                _ => None,
            })
            .min();
        let Some(oldest) = oldest else {
            bail!(
                "the ESP at {} has {} MiB free, and the boot image needs {} MiB with a margin; archix keeps only the current system's and the fallback slot's entries already, so free space on the ESP (other boot images, an old loader) or make it bigger",
                esp.display(),
                free >> 20,
                needed >> 20
            );
        };
        planned.retain(|p| p.entry != Entry::Snapshot(oldest));
        dropped.push(oldest);
    }
}

/// The snapshots that have an entry now, which prune keeps.
pub fn pinned(paths: &Paths, cap: u32) -> BTreeSet<u32> {
    let Ok(state) = load(&dir(paths)) else {
        return BTreeSet::new();
    };
    let records = crate::snapshot::Store::open(&paths.root).list().unwrap_or_default();
    let mut pinned: BTreeSet<u32> = with_entries(&state, &records, cap).into_iter().collect();
    pinned.extend(state.slot);
    pinned
}

/// What's missing of the boot step's work for the current generation on the ESP, if it can tell: the image, its entry, the rescue entry. An edited entry isn't missing, and a directory that isn't the ESP's content or can't be read says nothing.
pub fn boot_step_missing(paths: &Paths, plan: &BootPlan) -> Option<String> {
    if plan.loader != Loader::SystemdBoot {
        return None;
    }
    let esp = Path::new(&plan.esp);
    fs::read_dir(esp.join("EFI")).ok()?;
    let Some(image) = load(&dir(paths)).ok()?.current.main else {
        return Some("no boot image is recorded for it".to_string());
    };
    if !fs::metadata(esp.join(IMAGES).join(&image.name)).is_ok_and(|meta| meta.len() > 0) {
        return Some(format!("its boot image {} isn't on the ESP", image.name));
    }
    let entries = on_esp(esp);
    if !entries.contains_key(&Entry::Current.id()) {
        return Some("its boot entry isn't on the ESP".to_string());
    }
    if !loader_points_here(esp) {
        return Some("loader.conf doesn't point systemd-boot at archix's entries".to_string());
    }
    if plan.rescue && !super::rescue::entry_whole(esp) {
        return Some("the rescue entry isn't on the ESP".to_string());
    }
    None
}

/// Whether loader.conf has the default and preferred lines point_loader writes, which a cut while it was written over can leave out.
fn loader_points_here(esp: &Path) -> bool {
    let text = fs::read_to_string(esp.join("loader/loader.conf")).unwrap_or_default();
    let has = |key: &str, value: &str| {
        text.lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>() == [key, value])
    };
    has("default", CURRENT) && has("preferred", PREFERRED)
}

/// At boot: writes loader.conf's lines again when the current entry is on the ESP but loader.conf doesn't name it, as a cut while it was written over leaves it, so the next boot is archix's and counted; returns whether it did.
pub fn repair_loader(paths: &Paths, plan: &BootPlan) -> Result<bool> {
    let esp = Path::new(&plan.esp);
    if plan.loader != Loader::SystemdBoot || !on_esp(esp).contains_key(&Entry::Current.id()) {
        return Ok(false);
    }
    let copy = loader_copy(&dir(paths));
    if loader_points_here(esp) {
        // The user's edits since the last switch go into the copy too.
        boot::keep_loader_copy(esp, &copy)?;
        return Ok(false);
    }
    let signed = plan.secure_boot.unwrap_or_else(boot::sbctl_has_keys);
    boot::point_loader(esp, PREFERRED, CURRENT, signed, Some(&copy))?;
    Ok(true)
}

/// The last loader.conf archix wrote or saw whole, beside the menu's state.
fn loader_copy(dir: &Path) -> PathBuf {
    dir.join("loader.conf")
}

/// Checked before a switch changes anything: the ESP must have room for a new image the size of the current one, beside the images the current system and the slot keep.
pub fn precheck(paths: &Paths, plan: &BootPlan) -> Result<()> {
    if plan.loader == Loader::Limine {
        return Ok(());
    }
    let esp = Path::new(&plan.esp);
    let images = esp.join(IMAGES);
    let state = load(&dir(paths))?;
    let size = |name: &str| fs::metadata(images.join(name)).map_or(0, |meta| meta.len());
    let Some(current) = &state.current.main else {
        return Ok(());
    };
    let kept: BTreeSet<&str> = std::iter::once(current.name.as_str())
        .chain(
            state
                .slot
                .and_then(|slot| state.snapshots.get(&slot))
                .map(String::as_str),
        )
        .collect();
    let freeable: u64 = fs::read_dir(&images)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|file| !kept.contains(&*file.file_name().to_string_lossy()))
        .filter_map(|file| file.metadata().ok())
        .map(|meta| meta.len())
        .sum();
    let needed = size(&current.name) + boot::MARGIN;
    let measured = images.ancestors().find(|path| path.exists()).unwrap_or(esp);
    let free = boot::free_bytes(measured)?;
    if free + freeable < needed {
        bail!(
            "the ESP at {} has {} MiB free ({} MiB more once older snapshots' entries go), but a new boot image needs about {} MiB; free up space on it",
            esp.display(),
            free >> 20,
            freeable >> 20,
            needed >> 20
        );
    }
    Ok(())
}

/// Writes the menu again from the state, the snapshots there are and `plan`: after a switch or rollback with `reset`, which gives the current system's entry its tries again, or after snapshots came or went. On Limine nothing is written; GRUB gets its menu file instead of entries.
pub fn refresh(paths: &Paths, plan: &BootPlan, cap: Option<u32>, reset: bool) -> Result<()> {
    let dir = dir(paths);
    let mut state = load(&dir)?;
    if reset && let Some(failed) = super::health::failed_while_good() {
        state.blessed_failed_units = failed;
    }
    let records = match cap {
        Some(_) => crate::snapshot::Store::open(&paths.root).list()?,
        None => Vec::new(),
    };
    let existing: BTreeSet<u32> = records.iter().map(|record| record.number).collect();
    forget_missing(&mut state, &existing);
    let esp = Path::new(&plan.esp);
    let mut planned = self::plan(&state, &records, cap.unwrap_or(0));
    let image_size = |name: &str| {
        [
            esp.join(IMAGES).join(name),
            paths.root.join(boot::BUILT.trim_start_matches('/')).join(name),
        ]
        .iter()
        .find_map(|path| fs::metadata(path).ok())
        .map_or(0, |meta| meta.len())
    };
    match plan.loader {
        Loader::Limine => {
            save(&dir, &state)?;
            return Ok(());
        }
        Loader::SystemdBoot | Loader::Grub => {}
    }
    fs::create_dir_all(esp.join(IMAGES)).with_context(|| format!("creating {}", esp.join(IMAGES).display()))?;
    let built_dir = paths.root.join(boot::BUILT.trim_start_matches('/'));
    for (number, image) in drop_unreachable(esp, &built_dir, &mut planned, state.slot)? {
        eprintln!(
            "warning: snapshot {number}'s boot image {image} is neither on the ESP nor among the images this root built, so it has no boot entry"
        );
    }
    let free = || boot::free_bytes(&esp.join(IMAGES));
    let dropped = fit(esp, &dir, &mut planned, &image_size, &free)?;
    if !dropped.is_empty() {
        let numbers: Vec<String> = dropped.iter().map(u32::to_string).collect();
        eprintln!(
            "warning: the ESP is short of room, so snapshots {} lose their boot entries",
            numbers.join(", ")
        );
    }
    for name in missing_images(esp, &planned) {
        let built = paths.root.join(boot::BUILT.trim_start_matches('/')).join(name);
        if !built.is_file() {
            bail!("the boot image {name} is neither on the ESP nor freshly built");
        }
        crate::testhooks::mark("boot-image-copy");
        crate::durable::copy_esp(&built, &esp.join(IMAGES).join(name))?;
        crate::testhooks::mark("boot-image-copied");
    }
    crate::durable::sync_esp(esp)?;
    let counted = plan.tries > 0
        && (boot::loader_counts_boots(crate::testhooks::loader_features())
            || super::limine::next_boot_is_archix_systemd_boot(paths, plan.loader, esp));
    let signed = plan.secure_boot.unwrap_or_else(boot::sbctl_has_keys);
    match plan.loader {
        Loader::SystemdBoot => write(
            &Write {
                esp,
                dir: &dir,
                counted,
                tries: plan.tries,
                reset,
                signed,
            },
            &planned,
        )?,
        Loader::Grub => super::grub::write(paths, esp, &dir, &planned, &plan.cmdline)?,
        Loader::Limine => {}
    }
    crate::durable::write(&paths.state.join("boot").join("esp"), plan.esp.as_bytes())?;
    save(&dir, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_step_is_whole_with_the_current_image_and_entry_and_the_rescue_entry_if_wanted() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path().join("esp");
        let paths = Paths::under(&tmp.path().join("root"));
        let plan = |rescue: bool| -> BootPlan {
            serde_json::from_value(serde_json::json!({
                "esp": esp, "kernel": "linux", "microcode": [], "cmdline": "", "secure_boot": false, "rescue": rescue,
            }))
            .unwrap()
        };
        assert_eq!(boot_step_missing(&paths, &plan(true)), None, "not an ESP with content");
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::create_dir_all(esp.join("loader/entries")).unwrap();
        assert!(boot_step_missing(&paths, &plan(true)).is_some_and(|why| why.contains("no boot image")));
        let state = State {
            current: Images {
                main: Some(Image {
                    name: "a.efi".into(),
                    version: "1".into(),
                }),
                extras: Vec::new(),
            },
            ..Default::default()
        };
        save(&dir(&paths), &state).unwrap();
        assert!(boot_step_missing(&paths, &plan(true)).is_some_and(|why| why.contains("a.efi")));
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        assert!(boot_step_missing(&paths, &plan(true)).is_some_and(|why| why.contains("boot entry")));
        fs::write(esp.join("loader/entries/archix-current+2-0.conf"), "edited").unwrap();
        assert!(boot_step_missing(&paths, &plan(true)).is_some_and(|why| why.contains("loader.conf")));
        fs::write(
            esp.join("loader/loader.conf"),
            "timeout 3\ndefault archix-current.conf\npreferred archix-*\n",
        )
        .unwrap();
        assert!(boot_step_missing(&paths, &plan(true)).is_some_and(|why| why.contains("rescue")));
        assert_eq!(boot_step_missing(&paths, &plan(false)), None);
        fs::create_dir_all(esp.join(crate::system::rescue::DIR)).unwrap();
        fs::write(esp.join(crate::system::rescue::DIR).join("r.efi"), "r").unwrap();
        fs::write(
            esp.join("loader/entries").join(crate::system::rescue::ENTRY),
            crate::system::rescue::entry_text("r.efi", "1"),
        )
        .unwrap();
        assert_eq!(boot_step_missing(&paths, &plan(true)), None);
    }

    fn record(number: u32, kind: Kind) -> Record {
        Record {
            format: crate::snapshot::FORMAT,
            number,
            kind,
            created: 1_792_576_985,
            tree: None,
            commit: None,
            generation: Some(number),
            packages: BTreeMap::new(),
            label: String::new(),
            note: None,
        }
    }

    fn image(name: &str) -> Image {
        Image {
            name: name.to_string(),
            version: "6.17.1-arch1-1".to_string(),
        }
    }

    #[test]
    fn entry_ids_name_the_current_system_its_extra_images_and_snapshots() {
        for entry in [
            Entry::Current,
            Entry::Extra("protected".into()),
            Entry::Extra("linux-lts".into()),
            Entry::Snapshot(7),
        ] {
            assert_eq!(Entry::parse(&entry.id()), Some(entry));
        }
        assert_eq!(Entry::parse("archix-current+2-1.conf"), Some(Entry::Current));
        assert_eq!(Entry::parse("archix-snapshot-12+0-1.conf"), Some(Entry::Snapshot(12)));
        for other in [
            "archix-rescue.conf",
            "archix-snapshot-.conf",
            "archix-snapshot-07.conf",
            "archix-snapshot-x.conf",
            "archix-current-.conf",
            "archix-3.conf",
            "arch-linux.conf",
            "archix-current.efi",
        ] {
            assert_eq!(Entry::parse(other), None, "{other}");
        }
    }

    #[test]
    fn the_slot_is_the_first_snapshot_since_the_last_good_boot_and_moves_only_after_one() {
        let mut state = State {
            current: Images {
                main: Some(image("a.efi")),
                extras: Vec::new(),
            },
            ..State::default()
        };
        taken(&mut state, 1);
        assert_eq!(
            (state.slot, state.snapshots.get(&1).map(String::as_str)),
            (Some(1), Some("a.efi"))
        );
        state.current.main = Some(image("b.efi"));
        taken(&mut state, 2);
        taken(&mut state, 3);
        assert_eq!(
            state.slot,
            Some(1),
            "a switch, pacman -Syu and yay in a row keep the slot"
        );
        state.armed = true;
        assert_eq!(
            state.slot,
            Some(1),
            "a good boot keeps the slot until the next snapshot"
        );
        taken(&mut state, 4);
        assert_eq!((state.slot, state.armed), (Some(4), false));
        forget_missing(&mut state, &BTreeSet::from([1, 2, 3]));
        assert_eq!(state.slot, None, "a slot whose snapshot is gone is free");
        assert!(!state.snapshots.contains_key(&4));
    }

    #[test]
    fn switch_snapshots_and_pacman_ones_that_changed_the_image_get_entries_up_to_the_cap_plus_the_slot() {
        let mut state = State {
            current: Images {
                main: Some(image("d.efi")),
                extras: Vec::new(),
            },
            slot: Some(1),
            ..State::default()
        };
        for (number, name) in [(1, "a"), (2, "a"), (3, "b"), (4, "b"), (5, "c"), (6, "c"), (7, "d")] {
            state.snapshots.insert(number, format!("{name}.efi"));
        }
        let records = vec![
            record(1, Kind::Switch),
            record(2, Kind::Pacman),
            record(3, Kind::Pacman),
            record(4, Kind::Switch),
            record(5, Kind::Manual),
            record(6, Kind::Pacman),
            record(7, Kind::Pacman),
            record(8, Kind::Switch),
        ];
        assert_eq!(
            with_entries(&state, &records, 5),
            [1, 2, 4, 6],
            "2 and 6 came before a new image, 3 and 7 didn't; 5 is manual; 8 has no image on record"
        );
        assert_eq!(with_entries(&state, &records, 2), [1, 4, 6], "the cap leaves the slot");
        let planned = plan(&state, &records, 2);
        let order: Vec<(Entry, &str)> = planned.iter().map(|p| (p.entry.clone(), p.sort_key.as_str())).collect();
        assert_eq!(
            order,
            [
                (Entry::Current, SORT_CURRENT),
                (Entry::Snapshot(6), SORT_SNAPSHOT),
                (Entry::Snapshot(4), SORT_SNAPSHOT),
                (Entry::Snapshot(1), SORT_SLOT),
            ]
        );
        assert!(SORT_CURRENT < SORT_SLOT && SORT_SLOT < SORT_SNAPSHOT && SORT_SNAPSHOT < SORT_EXTRA);
        assert!(SORT_EXTRA < crate::system::rescue::SORT_KEY);
    }

    fn planned(entry: Entry, image: &str) -> Planned {
        Planned {
            entry,
            image: image.to_string(),
            title: "t".into(),
            sort_key: SORT_SNAPSHOT.into(),
            version: "1".into(),
        }
    }

    #[test]
    fn a_change_gives_the_current_entry_its_tries_again_and_other_entries_keep_their_counters() {
        let current = planned(Entry::Current, "a.efi");
        let snapshot = planned(Entry::Snapshot(3), "a.efi");
        let bad = "archix-current+0-2.conf".to_string();
        assert_eq!(file_name(&current, Some(&bad), true, 2, true), "archix-current+2.conf");
        assert_eq!(file_name(&current, Some(&bad), true, 2, false), bad);
        assert_eq!(file_name(&current, None, true, 2, false), "archix-current+2.conf");
        assert_eq!(file_name(&current, Some(&bad), false, 2, true), "archix-current.conf");
        assert_eq!(file_name(&snapshot, None, true, 2, true), "archix-snapshot-3+1.conf");
        let blessed = "archix-snapshot-3.conf".to_string();
        assert_eq!(file_name(&snapshot, Some(&blessed), true, 2, true), blessed);
        assert_eq!(
            file_name(&planned(Entry::Extra("protected".into()), "b.efi"), None, true, 2, true),
            "archix-current-protected.conf"
        );
    }

    #[test]
    fn the_state_moves_into_archix_once_it_is_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let paths = Paths {
            state: tmp.path().join("state"),
            ..Paths::under(&root)
        };
        let old = paths.state.join("boot");
        fs::create_dir_all(old.join("entries")).unwrap();
        fs::write(old.join("entries").join(CURRENT), "title t\n").unwrap();
        save(
            &old,
            &State {
                armed: true,
                ..State::default()
            },
        )
        .unwrap();
        adopt(&paths).unwrap();
        assert_eq!(dir(&paths), old, "a root without @archix keeps it in the archix state");
        fs::create_dir_all(root.join(".archix")).unwrap();
        let mounted = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "tmpfs"])
            .arg(root.join(".archix"))
            .status()
            .is_ok_and(|status| status.success());
        if !mounted {
            return;
        }
        let moved = (|| {
            adopt(&paths)?;
            let new = dir(&paths);
            anyhow::Ok((new.clone(), load(&new)?, new.join("entries").join(CURRENT).exists()))
        })();
        let _ = std::process::Command::new("umount").arg(root.join(".archix")).status();
        let (new, state, entry) = moved.unwrap();
        assert_eq!(new, root.join(".archix/boot"));
        assert!(state.armed && entry, "the state and the entry records moved");
    }

    #[test]
    fn writing_the_menu_records_each_entry_drops_others_and_their_images_and_sets_preferred() {
        let tmp = tempfile::tempdir().unwrap();
        let (esp, dir) = (tmp.path().join("esp"), tmp.path().join("state"));
        fs::create_dir_all(esp.join("loader/entries")).unwrap();
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        for name in ["a.efi", "b.efi", "old.efi"] {
            fs::write(esp.join(IMAGES).join(name), name).unwrap();
        }
        fs::write(esp.join("loader/entries/archix-snapshot-1+0-1.conf"), "x").unwrap();
        fs::write(esp.join("loader/entries/archix-snapshot-2.conf"), "x").unwrap();
        fs::write(esp.join("loader/entries/windows.conf"), "x").unwrap();
        fs::write(esp.join("loader/loader.conf"), "timeout 3\ndefault windows.conf\n").unwrap();
        let mut current = planned(Entry::Current, "a.efi");
        current.sort_key = SORT_CURRENT.into();
        let menu = vec![current, planned(Entry::Snapshot(2), "b.efi")];
        let how = Write {
            esp: &esp,
            dir: &dir,
            counted: true,
            tries: 2,
            reset: true,
            signed: false,
        };
        write(&how, &menu).unwrap();
        let mut names: Vec<String> = fs::read_dir(esp.join("loader/entries"))
            .unwrap()
            .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // snapshot 2's text changed, and a blessed entry has no second spelling, so it's written counted rather than over itself.
        assert_eq!(
            names,
            ["archix-current+2.conf", "archix-snapshot-2+1.conf", "windows.conf"]
        );
        assert!(!esp.join(IMAGES).join("old.efi").exists());
        assert!(esp.join(IMAGES).join("b.efi").exists());
        assert_eq!(
            fs::read_to_string(record_path(&dir, "archix-current+2.conf")).unwrap(),
            fs::read_to_string(esp.join("loader/entries/archix-current+2.conf")).unwrap()
        );
        assert!(!record_path(&dir, "archix-snapshot-1.conf").exists());
        let loader = fs::read_to_string(esp.join("loader/loader.conf")).unwrap();
        assert!(loader.contains("timeout 3"), "{loader}");
        assert!(
            loader.contains("preferred archix-*\n") && loader.contains("default archix-current.conf\n"),
            "{loader}"
        );
        assert!(!loader.contains("windows"), "{loader}");
        let mut size_planned = menu.clone();
        size_planned.push(planned(Entry::Snapshot(1), "huge.efi"));
        let size = |name: &str| if name == "huge.efi" { 1 << 30 } else { 1 };
        let free = || Ok(64 << 20);
        assert_eq!(
            fit(&esp, &dir, &mut size_planned, &size, &free).unwrap(),
            [1],
            "the oldest snapshot entry goes when the ESP is short"
        );
        let mut slot_only = vec![Planned {
            sort_key: SORT_SLOT.into(),
            ..planned(Entry::Snapshot(1), "huge.efi")
        }];
        let err = fit(&esp, &dir, &mut slot_only, &size, &free).unwrap_err().to_string();
        assert!(err.contains("MiB free"), "{err}");
    }

    #[test]
    fn a_cut_between_writing_an_entry_and_removing_its_old_name_leaves_one_entry_after_the_next_write() {
        let tmp = tempfile::tempdir().unwrap();
        let (esp, dir) = (tmp.path().join("esp"), tmp.path().join("state"));
        fs::create_dir_all(esp.join("loader/entries")).unwrap();
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        for name in [
            "archix-current.conf",
            "archix-current+2.conf",
            "archix-current+0-2.conf",
        ] {
            fs::write(esp.join("loader/entries").join(name), "x").unwrap();
        }
        let how = Write {
            esp: &esp,
            dir: &dir,
            counted: true,
            tries: 2,
            reset: true,
            signed: false,
        };
        write(&how, &[planned(Entry::Current, "a.efi")]).unwrap();
        let names: Vec<String> = fs::read_dir(esp.join("loader/entries"))
            .unwrap()
            .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // Which old name the write starts from goes by the directory's order, which ext4's hashing varies, and either spelling counts two tries.
        assert!(
            names == ["archix-current+2.conf"] || names == ["archix-current+2-0.conf"],
            "{names:?}"
        );
    }

    fn whole(esp: &Path) -> String {
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        format!("title t\nversion 1\nsort-key {SORT_CURRENT}\nuki /{IMAGES}/a.efi\n")
    }

    #[test]
    fn of_two_names_for_one_entry_the_whole_one_stays_the_booted_or_counted_one_first() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path();
        let entries = esp.join("loader/entries");
        fs::create_dir_all(&entries).unwrap();
        let text = whole(esp);
        let names = |dir: &Path| {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        for name in [
            "archix-current.conf",
            "archix-current+1-1.conf",
            "archix-snapshot-2+1.conf",
            "windows.conf",
        ] {
            fs::write(entries.join(name), &text).unwrap();
        }
        assert_eq!(
            drop_second_names(esp, Some("archix-current+1-1.conf")).unwrap(),
            ["archix-current.conf"]
        );
        assert_eq!(
            names(&entries),
            ["archix-current+1-1.conf", "archix-snapshot-2+1.conf", "windows.conf"]
        );
        fs::write(entries.join("archix-current.conf"), &text).unwrap();
        assert_eq!(
            drop_second_names(esp, None).unwrap(),
            ["archix-current.conf"],
            "the counted one stays"
        );
        assert!(drop_second_names(esp, None).unwrap().is_empty());
        // What fsck.fat left at P49: the uncounted name emptied, the counted one whole.
        fs::write(entries.join("archix-current.conf"), "").unwrap();
        fs::write(entries.join("archix-current+2.conf"), &text).unwrap();
        fs::remove_file(entries.join("archix-current+1-1.conf")).unwrap();
        assert_eq!(
            drop_second_names(esp, Some("archix-current.conf")).unwrap(),
            ["archix-current.conf"],
            "a torn name goes, even the booted one"
        );
        fs::write(entries.join("archix-current.conf"), "").unwrap();
        fs::write(entries.join("archix-current+2.conf"), "title t\n\0\0").unwrap();
        assert!(
            drop_second_names(esp, None).unwrap().is_empty(),
            "with none whole, both stay for the next refresh to rewrite"
        );
    }

    #[test]
    fn a_snapshot_the_menu_gives_an_entry_but_the_esp_lacks_or_holds_torn_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path();
        let text = whole(esp);
        let entries = esp.join("loader/entries");
        fs::create_dir_all(&entries).unwrap();
        let state = State {
            snapshots: BTreeMap::from([
                (1, "a.efi".to_string()),
                (2, "a.efi".to_string()),
                (3, "a.efi".to_string()),
            ]),
            slot: Some(1),
            ..State::default()
        };
        let records = [
            record(1, Kind::Switch),
            record(2, Kind::Switch),
            record(3, Kind::Switch),
        ];
        fs::write(entries.join("archix-snapshot-1+1.conf"), &text).unwrap();
        fs::write(entries.join("archix-snapshot-2+1.conf"), "").unwrap();
        assert_eq!(missing_entries(esp, &state, &records, 5), [2, 3]);
    }

    #[test]
    fn writing_the_menu_clears_a_temporary_entry_a_cut_left() {
        let tmp = tempfile::tempdir().unwrap();
        let (esp, dir) = (tmp.path().join("esp"), tmp.path().join("state"));
        fs::create_dir_all(esp.join("loader/entries")).unwrap();
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        fs::write(esp.join("loader/entries/archix-current+2-0.conf.tmp"), "torn").unwrap();
        let how = Write {
            esp: &esp,
            dir: &dir,
            counted: true,
            tries: 2,
            reset: true,
            signed: false,
        };
        write(&how, &[planned(Entry::Current, "a.efi")]).unwrap();
        let names: Vec<String> = fs::read_dir(esp.join("loader/entries"))
            .unwrap()
            .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["archix-current+2.conf"]);
    }

    #[test]
    fn a_counted_name_has_a_second_spelling_systemd_boot_reads_alike() {
        assert_eq!(
            alternate("archix-snapshot-1+1.conf").as_deref(),
            Some("archix-snapshot-1+1-0.conf")
        );
        assert_eq!(
            alternate("archix-snapshot-1+1-0.conf").as_deref(),
            Some("archix-snapshot-1+1.conf")
        );
        assert_eq!(
            alternate("archix-current+0-2.conf"),
            None,
            "a tried entry has no second spelling"
        );
        assert_eq!(alternate("archix-current+1-1.conf"), None);
        assert_eq!(alternate("archix-current.conf"), None);
        assert_eq!(boot::entry_id("archix-snapshot-1+1-0.conf"), "archix-snapshot-1.conf");
    }

    #[test]
    fn a_changed_entry_is_written_under_a_name_the_esp_doesnt_have_and_an_unchanged_one_is_left_alone() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let (esp, dir) = (tmp.path().join("esp"), tmp.path().join("state"));
        fs::create_dir_all(esp.join("loader/entries")).unwrap();
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        let how = Write {
            esp: &esp,
            dir: &dir,
            counted: true,
            tries: 2,
            reset: false,
            signed: false,
        };
        let mut current = planned(Entry::Current, "a.efi");
        current.sort_key = SORT_CURRENT.into();
        let slot = |n: u32| Planned {
            sort_key: SORT_SLOT.into(),
            ..planned(Entry::Snapshot(n), "a.efi")
        };
        write(&how, &[current.clone(), slot(1)]).unwrap();
        let names = || {
            let mut names: Vec<String> = fs::read_dir(esp.join("loader/entries"))
                .unwrap()
                .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(), ["archix-current+2.conf", "archix-snapshot-1+1.conf"]);
        let inode = |name: &str| fs::metadata(esp.join("loader/entries").join(name)).unwrap().ino();
        let current_inode = inode("archix-current+2.conf");
        // A newer snapshot becomes the slot, so snapshot 1's sort key changes, as at P48.
        write(&how, &[current.clone(), planned(Entry::Snapshot(1), "a.efi"), slot(2)]).unwrap();
        assert_eq!(
            names(),
            [
                "archix-current+2.conf",
                "archix-snapshot-1+1-0.conf",
                "archix-snapshot-2+1.conf"
            ]
        );
        assert!(
            fs::read_to_string(esp.join("loader/entries/archix-snapshot-1+1-0.conf"))
                .unwrap()
                .contains(&format!("sort-key {SORT_SNAPSHOT}\n")),
            "the changed text is under the new name"
        );
        assert_eq!(
            inode("archix-current+2.conf"),
            current_inode,
            "an unchanged entry isn't touched"
        );
        write(&how, &[current, slot(1), planned(Entry::Snapshot(2), "a.efi")]).unwrap();
        assert_eq!(
            names(),
            [
                "archix-current+2.conf",
                "archix-snapshot-1+1.conf",
                "archix-snapshot-2+1-0.conf"
            ],
            "and back to the first spelling"
        );
        assert!(!names().iter().any(|name| name.ends_with(".tmp")));
    }

    #[test]
    fn a_snapshot_entry_whose_image_is_nowhere_is_dropped_and_the_current_one_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (esp, built) = (tmp.path().join("esp"), tmp.path().join("built"));
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::create_dir_all(&built).unwrap();
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        fs::write(built.join("b.efi"), "b").unwrap();
        let mut planned = vec![
            planned(Entry::Current, "gone.efi"),
            planned(Entry::Snapshot(2), "a.efi"),
            planned(Entry::Snapshot(3), "b.efi"),
            planned(Entry::Snapshot(4), "gone.efi"),
        ];
        assert_eq!(
            drop_unreachable(&esp, &built, &mut planned, Some(2)).unwrap(),
            [(4, "gone.efi".to_string())]
        );
        assert_eq!(planned.len(), 3);
    }

    #[test]
    fn with_no_current_entry_left_on_the_esp_a_slot_without_its_image_is_dropped_rather_than_writing_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (esp, built) = (tmp.path().join("esp"), tmp.path().join("built"));
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::create_dir_all(&built).unwrap();
        fs::write(built.join("a.efi"), "a").unwrap();
        let mut planned = vec![
            planned(Entry::Current, "a.efi"),
            planned(Entry::Snapshot(4), "gone.efi"),
        ];
        assert_eq!(
            drop_unreachable(&esp, &built, &mut planned, Some(4)).unwrap(),
            [(4, "gone.efi".to_string())],
            "a wiped ESP, as before a live-USB recovery's keep: the current entry alone beats no menu"
        );
        assert_eq!(planned.len(), 1);
    }

    #[test]
    fn the_fallback_slots_entry_is_never_dropped_for_a_missing_image_the_refresh_fails_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (esp, built) = (tmp.path().join("esp"), tmp.path().join("built"));
        fs::create_dir_all(esp.join(IMAGES)).unwrap();
        fs::create_dir_all(&built).unwrap();
        fs::write(esp.join(IMAGES).join("a.efi"), "a").unwrap();
        fs::create_dir_all(esp.join("loader/entries")).unwrap();
        fs::write(
            esp.join("loader/entries/archix-current.conf"),
            "uki /EFI/archix/a.efi\n",
        )
        .unwrap();
        let mut planned = vec![
            planned(Entry::Current, "a.efi"),
            planned(Entry::Snapshot(3), "gone.efi"),
            planned(Entry::Snapshot(4), "gone.efi"),
        ];
        let error = format!(
            "{:#}",
            drop_unreachable(&esp, &built, &mut planned, Some(4)).unwrap_err()
        );
        assert!(
            error.contains("fallback slot, snapshot 4") && error.contains("gone.efi"),
            "{error}"
        );
        assert!(
            planned.iter().any(|p| p.entry == Entry::Snapshot(4)),
            "the slot's entry stays planned"
        );
    }
}
