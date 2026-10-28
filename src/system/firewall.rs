use std::collections::BTreeMap;
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

type Span = (i64, i64);

fn ranges(value: Option<&Value>) -> Result<Vec<Span>> {
    let mut out = Vec::new();
    for item in match value {
        Some(Value::List(items)) => items.as_slice(),
        _ => &[],
    } {
        match (get(item, "from"), get(item, "to")) {
            (Some(Value::Int(from)), Some(Value::Int(to))) if 1 <= *from && from <= to && *to <= 65535 => {
                out.push((*from, *to))
            }
            _ => bail!("firewall port ranges need from <= to, both 1-65535"),
        }
    }
    Ok(out)
}

/// Sorted spans with overlaps merged, as one nftables set can't hold overlapping intervals.
fn merged(mut spans: Vec<Span>, adjacent: bool) -> Vec<Span> {
    spans.sort_unstable();
    let mut out: Vec<Span> = Vec::new();
    for (from, to) in spans {
        match out.last_mut() {
            Some(last) if from <= last.1 + i64::from(adjacent) => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

fn set(spans: &[Span]) -> String {
    let items: Vec<String> = spans
        .iter()
        .map(|&(from, to)| {
            if from == to {
                from.to_string()
            } else {
                format!("{from}-{to}")
            }
        })
        .collect();
    items.join(", ")
}

/// The sets `sources = "lan"` rules match: private and link-local addresses.
const LAN4: &str = "archix_lan4";
const LAN6: &str = "archix_lan6";
pub const LAN_V4: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16"];
pub const LAN_V6: [&str; 2] = ["fc00::/7", "fe80::/10"];

/// Firewalls whose own tables would still drop what archix's opens.
pub const OTHERS: [&str; 2] = ["ufw.service", "firewalld.service"];

/// Refuses a deployment with archix's firewall that would also start ufw or firewalld, whether the config enables one or it's enabled here and the config doesn't disable it.
pub fn check_others(services: &BTreeMap<String, bool>, enabled_here: impl Fn(&str) -> bool) -> Result<()> {
    for unit in OTHERS {
        let why = match services.get(unit) {
            Some(true) => "the config enables it",
            Some(false) => continue,
            None if enabled_here(unit) => "it's enabled on this system, and the new deployment keeps it",
            None => continue,
        };
        bail!(
            "{unit} would run beside the archix firewall ({why}), and its rules would still drop what the firewall options open; set services[\"{unit}\"] = false to use archix's firewall, or firewall.enable = false to keep {}; nothing was changed",
            unit.trim_end_matches(".service")
        );
    }
    Ok(())
}

/// The services `allowed_services` names, sorted and once each, with whether they're open only to the LAN and their entries in `service_ports`.
fn services(firewall: &Value) -> Result<Vec<(&str, bool, &Value)>> {
    let mut names = std::collections::BTreeSet::new();
    if let Some(Value::List(items)) = get(firewall, "allowed_services") {
        for item in items {
            if let Value::Str(name) = item {
                names.insert(name.as_str());
            }
        }
    }
    let mut out = Vec::new();
    for name in names {
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("firewall service names are 1-64 letters, digits, '-', '_' or '.', not {name:?}");
        }
        match get(firewall, "service_ports").and_then(|ports| get(ports, name)) {
            Some(ports) => out.push((name, text(get(ports, "sources")) == "lan", ports)),
            None => bail!("firewall.allowed_services names {name:?}, which firewall.service_ports doesn't define"),
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
    let services = services(firewall)?;
    if services.iter().any(|&(_, lan, _)| lan) {
        let _ = writeln!(
            out,
            "  set {LAN4} {{\n    type ipv4_addr; flags interval;\n    elements = {{ {} }}\n  }}\n\n  \
             set {LAN6} {{\n    type ipv6_addr; flags interval;\n    elements = {{ {} }}\n  }}\n",
            LAN_V4.join(", "),
            LAN_V6.join(", ")
        );
    }
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
        let mut rules: Vec<(Option<&str>, bool, Vec<Span>)> = Vec::new();
        let mut explicit: Vec<Span> = ints(get(firewall, ports))?.into_iter().map(|p| (p, p)).collect();
        explicit.extend(ranges(get(firewall, spans))?);
        rules.push((None, false, explicit));
        for &(name, lan, entry) in &services {
            let mut open: Vec<Span> = ints(get(entry, proto))?.into_iter().map(|p| (p, p)).collect();
            open.extend(ranges(get(entry, &format!("{proto}_ranges")))?);
            rules.push((Some(name), lan, open));
        }
        // A port open to any source needs no LAN rule; one open to the LAN still needs a rule for any source.
        let (mut anywhere, mut on_lan): (Vec<Span>, Vec<Span>) = (Vec::new(), Vec::new());
        for (name, lan, spans) in rules {
            let mut spans = merged(spans, false);
            let opened = if lan { &on_lan } else { &anywhere };
            spans.retain(|&(from, to)| !opened.iter().any(|&(a, b)| a <= from && to <= b));
            if spans.is_empty() {
                continue;
            }
            let comment = name.map(|name| format!(" comment \"{name}\"")).unwrap_or_default();
            let ports = set(&spans);
            if lan {
                let _ = writeln!(out, "    ip saddr @{LAN4} {proto} dport {{ {ports} }} accept{comment}");
                let _ = writeln!(out, "    ip6 saddr @{LAN6} {proto} dport {{ {ports} }} accept{comment}");
            } else {
                let _ = writeln!(out, "    {proto} dport {{ {ports} }} accept{comment}");
                anywhere = merged([anywhere, spans.clone()].concat(), true);
            }
            on_lan = merged([on_lan, spans].concat(), true);
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
    fn services_get_a_commented_rule_each_without_ports_opened_before() {
        let rules = ruleset(&value(serde_json::json!({
            "enable": true,
            "allowed_tcp_ports": [8080, 22, 22],
            "allowed_services": ["syncthing", "ssh", "kdeconnect", "syncthing", "gsconnect"],
            "service_ports": {
                "ssh": { "tcp": [22] },
                "syncthing": { "tcp": [22000], "udp": [22000, 21027] },
                "kdeconnect": { "tcp_ranges": [{ "from": 1714, "to": 1764 }], "udp_ranges": [{ "from": 1714, "to": 1764 }], "tcp": [1720] },
                "gsconnect": { "tcp_ranges": [{ "from": 1714, "to": 1764 }] },
                "unused": { "tcp": [9] },
            },
        })))
        .unwrap()
        .unwrap();
        let input: Vec<&str> = rules
            .lines()
            .filter(|l| l.starts_with("    tcp dport") || l.starts_with("    udp dport"))
            .collect();
        assert_eq!(
            input,
            [
                "    tcp dport { 22, 8080 } accept",
                "    tcp dport { 1714-1764 } accept comment \"gsconnect\"",
                "    tcp dport { 22000 } accept comment \"syncthing\"",
                "    udp dport { 1714-1764 } accept comment \"kdeconnect\"",
                "    udp dport { 21027, 22000 } accept comment \"syncthing\"",
            ],
            "{rules}"
        );
        let unknown = ruleset(&value(
            serde_json::json!({ "enable": true, "allowed_services": ["nope"] }),
        ));
        assert!(unknown.unwrap_err().to_string().contains("\"nope\""));
        let quote = ruleset(&value(serde_json::json!({
            "enable": true,
            "allowed_services": ["a\"b"],
            "service_ports": { "a\"b": { "tcp": [1] } },
        })));
        assert!(quote.is_err(), "a name can't break out of the comment");
    }

    #[test]
    fn lan_services_admit_private_and_link_local_sources_on_both_families() {
        let rules = ruleset(&value(serde_json::json!({
            "enable": true,
            "allowed_udp_ports": [21027],
            "allowed_services": ["mdns", "syncthing", "wireguard"],
            "service_ports": {
                "mdns": { "udp": [5353], "sources": "lan" },
                "syncthing": { "tcp": [22000], "udp": [21027, 22000], "sources": "lan" },
                "wireguard": { "udp": [51820, 22000], "sources": "any" },
            },
        })))
        .unwrap()
        .unwrap();
        assert!(
            rules.contains(
                "  set archix_lan4 {\n    type ipv4_addr; flags interval;\n    elements = { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16 }\n  }\n"
            ) && rules.contains("    elements = { fc00::/7, fe80::/10 }\n"),
            "{rules}"
        );
        assert!(rules.find("set archix_lan6").unwrap() < rules.find("chain input").unwrap());
        let input: Vec<&str> = rules
            .lines()
            .filter(|l| l.contains(" dport {") && !l.contains("sport"))
            .collect();
        assert_eq!(
            input,
            [
                "    ip saddr @archix_lan4 tcp dport { 22000 } accept comment \"syncthing\"",
                "    ip6 saddr @archix_lan6 tcp dport { 22000 } accept comment \"syncthing\"",
                "    udp dport { 21027 } accept",
                "    ip saddr @archix_lan4 udp dport { 5353 } accept comment \"mdns\"",
                "    ip6 saddr @archix_lan6 udp dport { 5353 } accept comment \"mdns\"",
                "    ip saddr @archix_lan4 udp dport { 22000 } accept comment \"syncthing\"",
                "    ip6 saddr @archix_lan6 udp dport { 22000 } accept comment \"syncthing\"",
                "    udp dport { 22000, 51820 } accept comment \"wireguard\"",
            ],
            "{rules}"
        );
        let any = ruleset(&value(serde_json::json!({
            "enable": true,
            "allowed_services": ["ssh"],
            "service_ports": { "ssh": { "tcp": [22], "sources": "any" } },
        })))
        .unwrap()
        .unwrap();
        assert!(!any.contains("archix_lan"), "no LAN sets without a LAN service: {any}");
    }

    #[test]
    fn ufw_or_firewalld_beside_archix_firewall_is_refused() {
        let services = |pairs: &[(&str, bool)]| -> BTreeMap<String, bool> {
            pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
        };
        let none = |_: &str| false;
        let ufw_here = |unit: &str| unit == "ufw.service";
        assert!(check_others(&services(&[]), none).is_ok());
        let err = check_others(&services(&[]), ufw_here).unwrap_err().to_string();
        assert!(
            err.contains("ufw.service would run beside the archix firewall (it's enabled on this system")
                && err.contains("services[\"ufw.service\"] = false"),
            "{err}"
        );
        assert!(check_others(&services(&[("ufw.service", false)]), ufw_here).is_ok());
        let err = check_others(&services(&[("firewalld.service", true)]), none)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("firewalld.service would run beside the archix firewall (the config enables it)"),
            "{err}"
        );
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
