use std::fmt::Write as _;

use anyhow::{Result, bail};

use crate::value::Value;

pub const RULES: &str = "/etc/nftables-archix.conf";
pub const UNIT: &str = "archix-firewall.service";

/// The unit that loads archix's table and removes only that table when stopped, unlike nftables.service's flush.
pub fn unit() -> String {
    format!(
        "[Unit]\nDescription=archix firewall\nWants=network-pre.target\nBefore=network-pre.target\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/nft -f {RULES}\n\
         ExecReload=/usr/bin/nft -f {RULES}\nExecStop=-/usr/bin/nft delete table inet archix\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    )
}

fn get<'a>(map: &'a Value, key: &str) -> Option<&'a Value> {
    match map {
        Value::Map(map) => map.get(key),
        _ => None,
    }
}

fn ints(value: Option<&Value>) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for item in match value {
        Some(Value::List(items)) => items.as_slice(),
        _ => &[],
    } {
        match item {
            Value::Int(n) if (1..=65535).contains(n) => out.push(*n),
            other => bail!("firewall ports must be 1-65535, not {other}"),
        }
    }
    Ok(out)
}

fn ranges(value: Option<&Value>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in match value {
        Some(Value::List(items)) => items.as_slice(),
        _ => &[],
    } {
        match (get(item, "from"), get(item, "to")) {
            (Some(Value::Int(from)), Some(Value::Int(to))) if 1 <= *from && from <= to && *to <= 65535 => {
                out.push(format!("{from}-{to}"))
            }
            _ => bail!("firewall port ranges need from <= to, both 1-65535"),
        }
    }
    Ok(out)
}

fn text(value: Option<&Value>) -> &str {
    match value {
        Some(Value::Str(s)) => s,
        _ => "",
    }
}

/// Checks a ruleset with `nft -c` before a switch changes anything; skipped until nft is installed.
pub fn validate(rules: &str) -> Result<()> {
    if !std::path::Path::new("/usr/bin/nft").exists() {
        return Ok(());
    }
    let tmp = std::env::temp_dir().join(format!("archix-firewall-{}.nft", std::process::id()));
    std::fs::write(&tmp, rules)?;
    let output = std::process::Command::new("nft").arg("-c").arg("-f").arg(&tmp).output();
    let _ = std::fs::remove_file(&tmp);
    let output = output?;
    if !output.status.success() {
        bail!(
            "the firewall rules don't load (check extra_input_rules and extra_rules); nothing was changed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The nftables file for the `firewall` options, or None when it's off.
pub fn ruleset(firewall: &Value) -> Result<Option<String>> {
    if get(firewall, "enable") != Some(&Value::Bool(true)) {
        return Ok(None);
    }
    let mut out = String::from("#!/usr/bin/nft -f\n# Written by archix from the firewall options.\n");
    out.push_str("table inet archix\ndelete table inet archix\n\ntable inet archix {\n");
    match text(get(firewall, "check_reverse_path")) {
        "off" => {}
        mode => {
            let fib = if mode == "loose" {
                "fib saddr oif"
            } else {
                "fib saddr . iif oif"
            };
            let _ = writeln!(
                out,
                "  chain rpfilter {{\n    type filter hook prerouting priority mangle + 10; policy accept;\n    \
                 meta nfproto ipv4 udp sport . udp dport {{ 68 . 67, 67 . 68 }} accept\n    {fib} missing drop\n  }}\n"
            );
        }
    }
    out.push_str("  chain input {\n    type filter hook input priority filter; policy drop;\n");
    out.push_str("    ct state vmap { established : accept, related : accept, invalid : drop }\n");
    out.push_str("    iifname \"lo\" accept\n");
    let trusted: Vec<String> = match get(firewall, "trusted_interfaces") {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|i| match i {
                Value::Str(s) => Some(format!("\"{s}\"")),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if !trusted.is_empty() {
        let _ = writeln!(out, "    iifname {{ {} }} accept", trusted.join(", "));
    }
    out.push_str("    meta l4proto ipv6-icmp accept\n");
    out.push_str("    ip6 saddr fe80::/10 udp dport 546 accept\n");
    if get(firewall, "allow_ping") != Some(&Value::Bool(false)) {
        out.push_str("    icmp type echo-request accept\n");
    }
    for (proto, ports, spans) in [
        ("tcp", "allowed_tcp_ports", "allowed_tcp_port_ranges"),
        ("udp", "allowed_udp_ports", "allowed_udp_port_ranges"),
    ] {
        let mut open: Vec<String> = ints(get(firewall, ports))?.iter().map(i64::to_string).collect();
        open.extend(ranges(get(firewall, spans))?);
        if !open.is_empty() {
            let _ = writeln!(out, "    {proto} dport {{ {} }} accept", open.join(", "));
        }
    }
    for line in text(get(firewall, "extra_input_rules"))
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let _ = writeln!(out, "    {}", line.trim());
    }
    out.push_str("  }\n");
    for line in text(get(firewall, "extra_rules")).lines() {
        let _ = writeln!(out, "  {line}");
    }
    out.push_str("}\n");
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(json: serde_json::Value) -> Value {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => Value::Int(n.as_i64().unwrap()),
            serde_json::Value::String(s) => Value::Str(s),
            serde_json::Value::Array(items) => Value::List(items.into_iter().map(value).collect()),
            serde_json::Value::Object(map) => Value::Map(map.into_iter().map(|(k, v)| (k, value(v))).collect()),
        }
    }

    #[test]
    fn the_ruleset_opens_what_is_asked() {
        let rules = ruleset(&value(serde_json::json!({
            "enable": true,
            "allowed_tcp_ports": [22, 80],
            "allowed_udp_port_ranges": [{ "from": 60000, "to": 61000 }],
            "trusted_interfaces": ["tailscale0"],
            "allow_ping": false,
            "check_reverse_path": "loose",
            "extra_input_rules": "ip saddr 192.168.0.0/24 tcp dport 8080 accept\n",
        })))
        .unwrap()
        .unwrap();
        assert!(rules.contains("tcp dport { 22, 80 } accept"), "{rules}");
        assert!(rules.contains("udp dport { 60000-61000 } accept"), "{rules}");
        assert!(rules.contains("iifname { \"tailscale0\" } accept"), "{rules}");
        assert!(!rules.contains("echo-request"), "{rules}");
        assert!(rules.contains("fib saddr oif missing drop"), "{rules}");
        assert!(
            rules.contains("    ip saddr 192.168.0.0/24 tcp dport 8080 accept\n"),
            "{rules}"
        );
        assert!(!rules.contains("flush ruleset"), "other tables are left alone");
    }

    #[test]
    fn off_writes_nothing_and_bad_ports_are_rejected() {
        assert!(
            ruleset(&value(serde_json::json!({ "enable": false })))
                .unwrap()
                .is_none()
        );
        assert!(
            ruleset(&value(
                serde_json::json!({ "enable": true, "allowed_tcp_ports": [70000] })
            ))
            .is_err()
        );
    }
}
