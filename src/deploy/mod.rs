//! Deployments: each generation's usr, etc, opt, boot, pacman database and DKMS state as one tree under /archix/deploy, staged beside the running one and committed atomically.

pub mod commit;
pub mod flatten;
pub mod keep;
pub mod stage;
pub mod store;
pub mod view;
pub mod xattr;

pub use commit::{EspStep, SystemdBoot};
pub use keep::Keep;
pub use stage::{Staged, Staging};
pub use store::{Backend, Store};
pub use view::Transaction;

/// What every committed deployment holds.
pub const TREES: &[&str] = &["usr", "etc", "opt", "boot", "pacman", "dkms", "var-upper"];

/// The trees an overlay stages on filesystems without snapshots; the others are copied.
pub const OVERLAID: &[&str] = &["usr", "opt", "boot"];

/// The trees copied whole on filesystems without snapshots, so in-place edits and chmod can't reach the deployment they came from.
pub const COPIED: &[&str] = &["etc", "pacman", "dkms"];

/// So an upper layer holds only whole copied-up files, plain whiteouts and opaque directories, which flattening understands.
pub const OVERLAY_OPTIONS: &str = "metacopy=off,redirect_dir=off,index=off,xino=off";
