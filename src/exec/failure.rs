//! SCOPE 5's message for a switch that stopped partway: no automatic undo, the pre snapshot, the layout's rollback procedure, what it doesn't cover, and the journal.
use std::path::Path;

/// What the snapshot never covers on the v1 layout.
pub const NOT_COVERED: &str = "/home, /var/log, /var/cache, /srv, /root, /tmp and the ESP";

pub struct Failure<'a> {
    pub step: &'a str,
    pub error: &'a str,
    pub interrupted: bool,
    pub snapper_pre: Option<u32>,
    pub journal: &'a Path,
}

pub fn message(f: &Failure) -> String {
    let mut out = String::new();
    match f.interrupted {
        true => out.push_str(&format!(
            "archix switch was interrupted during {}: {}\n",
            f.step, f.error
        )),
        false => out.push_str(&format!("archix switch failed at {}: {}\n", f.step, f.error)),
    }
    out.push_str(
        "Nothing is undone automatically; the system is as far as the switch got, and pacman's lock is released.\n",
    );
    out.push_str(&format!("The journal of this switch is {}\n", f.journal.display()));
    match f.snapper_pre {
        Some(pre) => {
            out.push_str(&format!(
                "To go back to before this switch, pre snapshot {pre} (Limine + limine-snapper-sync):\n"
            ));
            out.push_str("  1. reboot;\n");
            out.push_str(&format!("  2. pick \"Snapshots → {pre}\" in the Limine menu;\n"));
            out.push_str(&format!(
                "  3. run `limine-snapper-restore --kernels {pre}`, restoring the kernels too, since the ESP isn't snapshotted.\n"
            ));
            out.push_str(&format!("The snapshot doesn't cover {NOT_COVERED}.\n"));
        }
        None => out.push_str("No pre snapshot was taken (--no-snapshot), so there is no rollback for this switch.\n"),
    }
    out.push_str("Or fix the cause and run `archix switch` again, which finishes what this one started.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_names_the_snapshot_the_procedure_what_it_misses_and_the_journal() {
        let text = message(&Failure {
            step: "services",
            error: "systemctl enable --now broken.service: failed",
            interrupted: false,
            snapper_pre: Some(41),
            journal: Path::new("/var/log/archix/switches/7"),
        });
        for part in [
            "failed at services",
            "Nothing is undone automatically",
            "pre snapshot 41",
            "reboot",
            "\"Snapshots → 41\" in the Limine menu",
            "limine-snapper-restore --kernels 41",
            "/home, /var/log, /var/cache, /srv, /root, /tmp and the ESP",
            "/var/log/archix/switches/7",
            "pacman's lock is released",
        ] {
            assert!(text.contains(part), "{part:?} missing from:\n{text}");
        }
        assert!(
            !text.contains("snapper rollback"),
            "never suggested with rootflags=subvol="
        );
    }

    #[test]
    fn without_a_snapshot_it_says_there_is_no_rollback() {
        let text = message(&Failure {
            step: "files",
            error: "x",
            interrupted: true,
            snapper_pre: None,
            journal: Path::new("/j"),
        });
        assert!(text.contains("interrupted during files"), "{text}");
        assert!(text.contains("no rollback"), "{text}");
        assert!(!text.contains("limine-snapper-restore"), "{text}");
    }
}
