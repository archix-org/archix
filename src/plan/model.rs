use serde::{Deserialize, Serialize};

use crate::alpm::PinMode;
use crate::date::Date;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub mode: PinMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<Date>,
    /// The lock's date when "never older than installed" moved it forward to `date`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<Date>,
    pub fingerprint: String,
    /// The sync dbs came from the cache, not a fetch at plan time.
    #[serde(default)]
    pub cached_dbs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PkgAction {
    Install,
    Upgrade,
    Downgrade,
    Remove,
    MarkExplicit,
    MarkDependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PkgReason {
    Declared,
    LockMoved,
    DriftInstalled,
    DriftRemoved,
    NoLongerDeclared,
    ConflictReplacement,
}

impl PkgReason {
    pub fn text(self) -> &'static str {
        match self {
            PkgReason::Declared => "declared",
            PkgReason::LockMoved => "lock moved",
            PkgReason::DriftInstalled => "drift-installed",
            PkgReason::DriftRemoved => "drift-removed",
            PkgReason::NoLongerDeclared => "no longer declared",
            PkgReason::ConflictReplacement => "conflict replacement",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageItem {
    pub action: PkgAction,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub reason: PkgReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AurItem {
    pub name: String,
    /// The AUR package base holding `name`; None when it's `name` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookItem {
    pub name: String,
    pub description: String,
    pub when: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileAction {
    Create,
    Modify,
    Delete,
    Mode,
    Owner,
    Unmanage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretStatus {
    Changed,
    Unchanged,
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileItem {
    pub action: FileAction,
    pub path: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretStatus>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hand_edited: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceAction {
    Enable,
    Disable,
    Mask,
    Unmask,
    Start,
    Restart,
    Reload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceItem {
    pub action: ServiceAction,
    pub unit: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub at_reboot: bool,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountAction {
    AddUser,
    ModifyUser,
    RemoveUser,
    AddGroup,
    ModifyGroup,
    RemoveGroup,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserItem {
    pub action: AccountAction,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineItem {
    pub line: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub schema_version: u32,
    pub header: Header,
    #[serde(default)]
    pub packages: Vec<PackageItem>,
    #[serde(default)]
    pub aur: Vec<AurItem>,
    #[serde(default)]
    pub hooks: Vec<HookItem>,
    #[serde(default)]
    pub files: Vec<FileItem>,
    #[serde(default)]
    pub services: Vec<ServiceItem>,
    #[serde(default)]
    pub users: Vec<UserItem>,
    #[serde(default)]
    pub home: Vec<LineItem>,
    #[serde(default)]
    pub flatpak: Vec<LineItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<LineItem>,
    #[serde(default)]
    pub refusals: Vec<String>,
    #[serde(default)]
    pub unchecked: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

pub mod exit {
    pub const NOTHING: u8 = 0;
    pub const CHANGES: u8 = 1;
    pub const REFUSES: u8 = 2;
    pub const ERROR: u8 = 3;
    pub const INCOMPLETE: u8 = 4;
}

impl Plan {
    pub fn new(header: Header) -> Plan {
        Plan {
            schema_version: SCHEMA_VERSION,
            header,
            packages: Vec::new(),
            aur: Vec::new(),
            hooks: Vec::new(),
            files: Vec::new(),
            services: Vec::new(),
            users: Vec::new(),
            home: Vec::new(),
            flatpak: Vec::new(),
            snapshot: None,
            refusals: Vec::new(),
            unchecked: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Whether anything would change; hooks and the snapshot only follow from other changes.
    pub fn has_changes(&self) -> bool {
        !(self.packages.is_empty()
            && self.aur.is_empty()
            && self.files.is_empty()
            && self.services.is_empty()
            && self.users.is_empty()
            && self.home.is_empty()
            && self.flatpak.is_empty())
    }

    pub fn has_package_transaction(&self) -> bool {
        !self.packages.is_empty() || !self.aur.is_empty()
    }

    /// SCOPE 3.1's exit code: refusal beats incomplete, which beats pending changes.
    pub fn exit_code(&self) -> u8 {
        if !self.refusals.is_empty() {
            exit::REFUSES
        } else if !self.unchecked.is_empty() {
            exit::INCOMPLETE
        } else if self.has_changes() {
            exit::CHANGES
        } else {
            exit::NOTHING
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            mode: PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: "f".into(),
            cached_dbs: false,
        }
    }

    fn line() -> LineItem {
        LineItem {
            line: "x".into(),
            reason: "declared".into(),
            risk: Vec::new(),
        }
    }

    #[test]
    fn exit_codes_follow_scope() {
        let mut plan = Plan::new(header());
        assert_eq!(plan.exit_code(), 0);
        plan.flatpak.push(line());
        assert_eq!(plan.exit_code(), 1);
        plan.unchecked.push("secret: not checked without root".into());
        assert_eq!(plan.exit_code(), 4);
        plan.refusals.push("no kernel".into());
        assert_eq!(plan.exit_code(), 2);
    }

    #[test]
    fn plan_round_trips_through_json_with_its_schema_version() {
        let mut plan = Plan::new(header());
        plan.packages.push(PackageItem {
            action: PkgAction::Upgrade,
            name: "glibc".into(),
            from: Some("2.40-1".into()),
            to: Some("2.41-1".into()),
            repo: Some("core".into()),
            reason: PkgReason::LockMoved,
            detail: None,
            risk: vec!["in the shared-library closure of pacman".into()],
        });
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"lock-moved\""));
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
    }
}
