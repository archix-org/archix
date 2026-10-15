use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The archix-owned module `archix install` and `archix remove` edit; it's imported automatically from its default place.
pub const FILE: &str = "installed.lua";

pub fn default_dir(config_dir: &Path, host: &str) -> PathBuf {
    config_dir.join("lua").join("hosts").join(host)
}

/// The packages listed in an installed.lua, in file order.
pub fn read(path: &Path) -> Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Some(start) = text.find("packages = {") else {
        bail!("{} has no `packages = {{ ... }}` list", path.display());
    };
    let body = &text[start + "packages = {".len()..];
    let Some(end) = body.find('}') else {
        bail!("{} has an unterminated packages list", path.display())
    };
    Ok(body[..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .filter(|name| !name.is_empty())
        .collect())
}

pub fn render(packages: &[String]) -> String {
    let mut out = String::from("-- Managed by `archix install` and `archix remove`.\nreturn {\n  packages = {\n");
    for name in packages {
        out.push_str(&format!("    {name:?},\n"));
    }
    out.push_str("  },\n}\n");
    out
}

/// The `require` name of `dir/installed.lua`, which only works for directories under the config's `lua/`.
pub fn module_name(config_dir: &Path, dir: &Path) -> Result<String> {
    let lua = config_dir
        .join("lua")
        .canonicalize()
        .with_context(|| format!("{} has no lua/ directory", config_dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} is not a directory", dir.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let Ok(relative) = dir.strip_prefix(&lua) else {
        bail!(
            "{} must be inside {} so the config can import it",
            dir.display(),
            lua.display()
        );
    };
    let mut parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.push("installed".to_string());
    Ok(parts.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_hand_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert!(read(&path).unwrap().is_empty());
        fs::write(&path, render(&["htop".into(), "yay-bin".into()])).unwrap();
        assert_eq!(read(&path).unwrap(), ["htop", "yay-bin"]);
        fs::write(&path, "return {\n  packages = { \"a\", -- kept\n \"b\" },\n}\n").unwrap();
        assert_eq!(read(&path).unwrap(), ["a", "b"]);
    }

    #[test]
    fn module_names_come_from_the_lua_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lua/hosts/box")).unwrap();
        fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        assert_eq!(
            module_name(dir.path(), &dir.path().join("lua/hosts/box")).unwrap(),
            "hosts.box.installed"
        );
        assert!(module_name(dir.path(), &dir.path().join("elsewhere")).is_err());
        assert!(module_name(dir.path(), &dir.path().join("missing")).is_err());
    }
}
