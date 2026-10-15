use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Keys archix sets in a file an app also writes, leaving the rest of it alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Merge {
    /// `ini`, `kconfig` or `json`.
    pub format: String,
    pub values: Value,
    /// kconfig only: mark each key `[$i]` so KDE treats it as locked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub immutable: bool,
}

impl Merge {
    /// `text` with the declared keys set.
    pub fn apply(&self, text: &str) -> Result<String> {
        match self.format.as_str() {
            "json" => {
                let mut current: Value = if text.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&strip_jsonc(text)).context("the file isn't valid JSON or JSONC")?
                };
                deep_merge(&mut current, &self.values);
                Ok(serde_json::to_string_pretty(&current)? + "\n")
            }
            "ini" | "kconfig" => {
                let kconfig = self.format == "kconfig";
                let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
                for (header, key, value) in entries(&self.values, kconfig)? {
                    let key = if kconfig && self.immutable {
                        format!("{key}[$i]")
                    } else {
                        key
                    };
                    set(&mut lines, header.as_deref(), &key, &value);
                }
                Ok(if lines.is_empty() {
                    String::new()
                } else {
                    lines.join("\n") + "\n"
                })
            }
            other => bail!("merge.format {other:?} isn't ini, kconfig or json"),
        }
    }

    /// Whether `text` already has every declared key at its declared value; JSON is compared by value, not layout.
    pub fn satisfied(&self, text: &str) -> bool {
        match self.format.as_str() {
            "json" => serde_json::from_str::<Value>(&strip_jsonc(text)).is_ok_and(|current| {
                self.apply(text)
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    == Some(current)
            }),
            _ => self
                .apply(text)
                .is_ok_and(|merged| merged.trim_end() == text.trim_end()),
        }
    }
}

/// JSONC (VS Code's settings, waybar's config) as plain JSON: comments and trailing commas removed; strings are left alone.
fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut in_string) = (0, false);
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|n| !n.is_whitespace())
                .is_some_and(|n| *n == '}' || *n == ']')
        {
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn deep_merge(into: &mut Value, from: &Value) {
    match (into, from) {
        (Value::Object(into), Value::Object(from)) => {
            for (key, value) in from {
                match into.get_mut(key) {
                    Some(existing) if existing.is_object() && value.is_object() => deep_merge(existing, value),
                    _ => {
                        into.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (into, from) => *into = from.clone(),
    }
}

fn scalar(value: &Value) -> Result<String> {
    Ok(match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items.iter().map(scalar).collect::<Result<Vec<_>>>()?.join(","),
        Value::Null => String::new(),
        Value::Object(_) => bail!("a nested table can't be a value here"),
    })
}

/// (section header, key, value) for every declared key; kconfig nests groups as [A][B], ini allows one level.
fn entries(values: &Value, kconfig: bool) -> Result<Vec<(Option<String>, String, String)>> {
    let Value::Object(top) = values else {
        bail!("merge.values must be a table");
    };
    let mut out = Vec::new();
    fn walk(
        path: &str,
        table: &serde_json::Map<String, Value>,
        kconfig: bool,
        out: &mut Vec<(Option<String>, String, String)>,
    ) -> Result<()> {
        for (key, value) in table {
            match value {
                Value::Object(inner) if kconfig || path.is_empty() => {
                    walk(&format!("{path}[{key}]"), inner, kconfig, out)?
                }
                Value::Object(_) => bail!("ini sections can't nest ({path}[{key}])"),
                value => out.push((
                    (!path.is_empty()).then(|| path.to_string()),
                    key.clone(),
                    scalar(value)?,
                )),
            }
        }
        Ok(())
    }
    walk("", top, kconfig, &mut out)?;
    Ok(out)
}

/// The key a line sets, without a kconfig `[$i]`/locale suffix, or None for headers, comments and blanks.
fn line_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with(['[', '#', ';']) {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    Some(key.trim_end().split('[').next().unwrap_or(key).trim_end())
}

fn set(lines: &mut Vec<String>, header: Option<&str>, key: &str, value: &str) {
    let start = match header {
        None => 0,
        Some(header) => match lines.iter().position(|line| line.trim() == header) {
            Some(at) => at + 1,
            None => {
                if lines.last().is_some_and(|line| !line.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(header.to_string());
                lines.push(format!("{key}={value}"));
                return;
            }
        },
    };
    let end = lines[start..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| start + offset);
    let bare = key.split('[').next().unwrap_or(key);
    if let Some(at) = (start..end).find(|at| line_key(&lines[*at]) == Some(bare)) {
        let spaced = lines[at].contains(" = ");
        lines[at] = if spaced {
            format!("{key} = {value}")
        } else {
            format!("{key}={value}")
        };
        return;
    }
    let mut insert = end;
    while insert > start && lines[insert - 1].trim().is_empty() {
        insert -= 1;
    }
    lines.insert(insert, format!("{key}={value}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merge(format: &str, values: Value) -> Merge {
        Merge {
            format: format.into(),
            values,
            immutable: false,
        }
    }

    #[test]
    fn kconfig_sets_keys_and_keeps_the_rest() {
        let file = "[General]\nColorScheme=Old\nfont=Noto\n\n[KDE]\nSingleClick=true\n";
        let m = merge(
            "kconfig",
            json!({ "General": { "ColorScheme": "Breeze" }, "KDE": { "AnimationDurationFactor": 0.5 }, "Icons": { "Theme": "Papirus" } }),
        );
        assert_eq!(
            m.apply(file).unwrap(),
            "[General]\nColorScheme=Breeze\nfont=Noto\n\n[KDE]\nSingleClick=true\nAnimationDurationFactor=0.5\n\n[Icons]\nTheme=Papirus\n"
        );
        assert!(!m.satisfied(file));
        assert!(m.satisfied(&m.apply(file).unwrap()));
    }

    #[test]
    fn immutable_kconfig_keys_carry_the_lock_marker() {
        let m = Merge {
            immutable: true,
            ..merge(
                "kconfig",
                json!({ "General": { "ColorScheme": "Breeze" }, "Plugins": { "Kwin": { "blur": true } } }),
            )
        };
        let out = m.apply("[General]\nColorScheme=Old\n").unwrap();
        assert_eq!(
            out,
            "[General]\nColorScheme[$i]=Breeze\n\n[Plugins][Kwin]\nblur[$i]=true\n"
        );
        assert!(m.satisfied(&out));
    }

    #[test]
    fn ini_keeps_global_keys_and_spacing() {
        let m = merge("ini", json!({ "theme": "dark", "main": { "font": "mono" } }));
        assert_eq!(
            m.apply("theme = light\n[main]\nfont = sans\nsize = 11\n").unwrap(),
            "theme = dark\n[main]\nfont = mono\nsize = 11\n"
        );
    }

    #[test]
    fn json_merges_deeply_and_compares_by_value() {
        let m = merge("json", json!({ "bar": { "position": "top" }, "theme": "dank" }));
        let file = "{\"bar\": {\"position\": \"bottom\", \"height\": 30}, \"other\": 1}";
        let out = m.apply(file).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value,
            json!({ "bar": { "position": "top", "height": 30 }, "other": 1, "theme": "dank" })
        );
        let rewritten = "{\"theme\":\"dank\",\"other\":1,\"bar\":{\"height\":30,\"position\":\"top\"}}";
        assert!(m.satisfied(rewritten), "the app's own formatting isn't drift");
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_read() {
        let m = merge("json", json!({ "editor.fontSize": 14 }));
        let file = "{\n  // the editor\n  \"editor.tabSize\": 2, /* two */\n  \"url\": \"https://x//y\",\n}\n";
        let value: Value = serde_json::from_str(&m.apply(file).unwrap()).unwrap();
        assert_eq!(
            value,
            json!({ "editor.tabSize": 2, "url": "https://x//y", "editor.fontSize": 14 })
        );
    }
}
