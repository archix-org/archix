# archix new scope: coordinator spec (draft 5, 2026-11-16)

Status: approved by the critic 2026-11-16. Draft 2 resolved the critic's B1–B9; draft 3 resolved N1–N5; draft 4 resolved N6–N9; draft 5 resolves N10–N11; folds in the critic's non-blocking notes and the user's 2026-11-16 answers (Lua→home-manager stays in v1, archix-git keeps building main, month criteria approved). Tags like [B1]/[N1] mark where each item is addressed.

## 1. What archix is

archix is a declarative layer over tools Arch users already trust. It is not a package manager and not a rollback system.

- Packages: libalpm configured exactly as pacman (2.2) for repo packages; AUR packages built with devtools in a clean chroot (2.4). The user's AUR helper isn't involved in switches; it stays the user's tool between them [N3].
- Rollback: snapper with snap-pac, plus limine-snapper-sync. v1 supports Limine + limine-snapper-sync only; grub-btrfs is out of v1 [B6].
- User config: built-in home modules, or home-manager declared from Lua (section 7). One of the two per user.
- archix itself owns only the config evaluation, the lock, the plan, and applying the plan.

Non-goals: atomicity, its own snapshots, boot entries or UKIs, a rescue system, pacman hooks, a strict mode, a custom layout, `archix uninstall`, automatic undo.

Removing archix means removing its package. The system it leaves is a normal Arch/CachyOS. What remains on disk:
- /var/lib/archix, /var/log/archix;
- the declared files;
- the home-manager or built-in home files.

archix never writes the system's pacman.conf or mirrorlist, unless the user declares them as managed files (ordinary `!` files). It does replace /var/lib/pacman/sync/*.db after its install transaction with the dbs it installed from, leaving the state `pacman -Syu` would [N2].

## 2. Package state: one repo state, the real local db, the real lock

### 2.1 Repo state [B1, B2]

- archix never uses `Include` for its own pacman config. It generates a complete pacman.conf from `pacman-conf` output: [options] verbatim, then each repo in the system's order with its Server lines.
- Pinning mode is chosen per system and shown in diff's header:
  - **archive**: every enabled repo is pinnable (stock Arch: core, extra, multilib). The servers are replaced by the Arch Linux Archive at date D.
  - **live**: any enabled repo can't be pinned and shadows or extends official ones (CachyOS, chaotic-aur). Servers are left as they are. The switch is a full upgrade against the sync dbs fetched at plan time, and the lock records versions only.
- Never older than installed: with follow=system (the only mode in v1 [Q4]), if any installed package is newer than D, the plan moves D forward to the earliest date whose dbs aren't older than what's installed, and shows the extra upgrades as "lock moved". The plan never installs an older repo state onto a newer system.
- Test: every URL pacman fetches (pacman.log + `--print-format %u` before commit) is a URL the plan names.

### 2.2 libalpm handle, DBPath and locking [B3, N1, N2]

- The handle takes every setting from `pacman-conf`: RootDir, DBPath, CacheDir, LogFile, GPGDir, HookDir (the system dir plus /etc/pacman.d/hooks/ and any configured dir, in pacman's order), DownloadUser, ParallelDownloads, SigLevel/LocalFileSigLevel/RemoteFileSigLevel, NoUpgrade/NoExtract, CheckSpace, UseSyslog, Architecture. LogFile is what makes [ALPM] lines exist for verify (a).
- Question callbacks answer only as planned: planned conflict replacements yes, everything else no (a PGP key import question answers no and fails cleanly).
- Signals: SIGINT and SIGTERM during commit interrupt after the current package, as pacman does. SIGHUP is ignored outright [N9].
- A dropped session can't kill a commit [N9]: all output during a switch goes through fallible writes that drop errors (EIO, EPIPE), never `println!`/`eprintln!`; libalpm callbacks catch every panic at the extern "C" boundary and never unwind; the journal is the record. Test: close the pty master during commit.
- Lock mechanism: archix creates /var/lib/pacman/db.lck itself with O_EXCL (as libalpm does) at step 3, runs both transactions with ALPM_TRANS_FLAG_NOLOCK, and unlinks it at step 8.
- Hooks inherit SNAP_PAC_SKIP=y.
- Sync dbs for the plan live in /var/cache/archix/sync/<fingerprint>/ (root) or $XDG_CACHE_HOME/archix/sync/ (user diff), fetched once in plan() and never re-synced during a switch.
- The local db is the real one.
- diff prepares with ALPM_TRANS_FLAG_NOLOCK, as `pacman -p` does, takes no lock, and says file conflicts and disk space are checked at commit.
- The switch's order and lock window [N3]:
  1. plan (no lock);
  2. AUR builds and `home-manager build` (no lock; can take hours); a failure refuses here [N7];
  3. take /var/lib/pacman/db.lck; recheck the local-db fingerprint against the plan, refuse if it moved;
  4. snapper pre snapshot, wait for its Limine entry;
  5. install transaction; then replace /var/lib/pacman/sync/*.db(.sig) with the plan's dbs [N2];
  6. files, users, services;
  7. removal transaction;
  8. snapper post snapshot, journal's [ALPM] window end, release the lock [N6];
  9. home activation (the store path built in step 2), flatpak;
  10. journal end.
- A concurrent pacman or paru during 3–7 fails on the lock as against any pacman.
- Live mode: every switch with a package transaction is a full upgrade; diff's header says so. A plan whose package groups are empty (only files, services, home) runs no package transaction and fetches no dbs beyond plan() [critic note].
- Tests:
  - concurrent `pacman -S` during diff and during switch, both orders;
  - parity: the same kernel, DKMS and glibc upgrades through the pacman CLI and through archix in two identical VMs give identical [ALPM]/[ALPM-SCRIPTLET] lines, hooks run, ESP contents and limine.conf, and `pacman -Qkk` is clean on both; plus SIGHUP mid-commit [N1];
  - `pacman -S <pkg with an exact-version dep>` straight after a live-mode switch downgrades nothing [N2].

### 2.3 Resolver [Q1]

- plan() resolves with libalpm itself (prepare, without commit), using the executor's exact settings from pacman-conf: IgnorePkg/IgnoreGroup/HoldPkg/NoUpgrade/NoExtract, repo order, SigLevel, Architecture including znver4.
- archix's own resolve/ code is used only for the lock's record and for AUR ordering, never to decide repo transactions.
- Test: for N target sets from real dbs, plan()'s add/remove set == `pacman -Sp` / `-Rp` with the same config.

### 2.4 AUR [B7, N3]

- archix calls devtools directly as root, for both helpers' users:
  - `mkarchroot -C <generated pacman.conf> <root> base-devel`, then copy the plan's sync dbs into the chroot;
  - `makechrootpkg -c -r <root> -U <unprivileged build user> -I <built AUR deps>`, never `-u`.
- Each AUR package is fetched at its pinned commit and built in dependency order before any repo change. makedepends never reach the host; the host's only AUR change is `-U` of the built files inside the install transaction.
- A failed build stops the switch before anything is written.
- VCS packages: diff shows "version known after build"; fidelity compares name + AUR commit.
- An AUR package the user upgraded with paru between switches: record the installed version; take the commit from the helper's clone only if it matches, otherwise mark it "unknown". Never rebuild an older commit over it [Q4].

### 2.5 One transaction, removals last [B7, B8]

The order is:
1. AUR builds (chroot);
2. one libalpm transaction: sync installs/upgrades plus the `-U` of built AUR packages, with conflict replacements inside it, approved only if planned;
3. files, users, services;
4. a second libalpm transaction for removals, as `-Rs` (never `-n`, so changed configs become .pacsave).

- Before each commit, the prepared add/remove lists are compared with the plan. Any difference aborts the transaction before commit [B4].
- archix-git is never upgraded inside its own switch. The plan lists it as "upgrade on next switch / pacman", and diff says so [Q1].

### 2.6 Hard refusals (certainly broken) [B8]

The plan refuses when it would leave:
- no kernel installed, or the running kernel's package removed without another;
- pacman, archix, the configured AUR helper, or the keyring or mirrorlist of any enabled repo removed;
- a declared user's login shell package removed, or a login shell not in /etc/shells;
- a sudoers file that fails `visudo -cf`.

Implicitly declared (can't be removed by "not declared"):
- archix, the helper, pacman, enabled repos' keyrings and mirrorlists;
- the running kernel, the bootloader package;
- snapper, snap-pac and limine-snapper-sync when they are installed.

## 3. The plan contract

There is one function, `plan(config, lock, system) -> Plan`. diff and switch both call it.

### 3.1 `archix diff`

- It's read-only and runs as a normal user.
- Output groups:
  - **header**: pinning mode, date or "live", fingerprint;
  - **packages**: install, upgrade, downgrade, remove, reason change. The reasons are declared, lock moved, drift-installed, drift-removed, no longer declared, conflict replacement;
  - **AUR**: name, commit, helper;
  - **hooks** that will fire, from the hooks' Targets against the transaction;
  - **files**: create/modify/delete/mode/owner, with a unified diff; hand edits go to `.archix-save`;
  - **services**: enable/disable/mask/unmask, start/restart/reload, and "applies at reboot";
  - **users and groups**;
  - **home**: built-in modules' file changes, or "home-manager switch on <flake>@<rev>";
  - **flatpak**: live remotes, flagged like live repos;
  - **snapshot**.
- Units from packages that aren't installed yet: their [Install] is read from the downloaded package file. If it can't be read, the line says "symlinks known after install".
- Never restarted live, shown as `! applies at reboot`:
  - the display manager, dbus, logind;
  - NetworkManager and networkd;
  - sshd when the session is over ssh;
  - user@.service, systemd-journald.
- Exit codes:
  - 0: nothing to do;
  - 1: changes pending;
  - 2: switch would refuse;
  - 3: error, including offline. A plan is never built from a stale cache without saying so;
  - 4: incomplete, because something wasn't checkable without root. The unchecked items are listed. Running processes' maps are among them, so the "in use" marker is partial.
- `--json`: the Plan with a schema version. `--stat`: names only.

### 3.2 Risk markers (`!`), derived mechanically [Q2]

A package line gets `!` if the package:
- owns files under /boot, /efi, /usr/lib/modules, /usr/lib/initcpio or /usr/lib/firmware;
- is in the shared-library closure (ldd) of pacman, sudo, systemd, login, agetty, the display manager and greeter, any declared login shell, mkinitcpio or limine. That closure catches zlib-ng-compat;
- is a DKMS module: a refusal when a kernel in the plan would have no headers for an installed DKMS module, `!` otherwise;
- is a GPU driver, a chwd-managed package, a keyring or a mirrorlist;
- is the running kernel's package;
- is a provider swap, conflict replacement or downgrade;
- is archix itself, the helper, base-devel or devtools;
- is pam or libxcrypt while a screen locker is running ("don't lock the screen until reboot").

A file line gets `!` if it's in this list:
- /etc/sudoers and sudoers.d/*;
- pam.d/*, security/*, nsswitch.conf, shells;
- passwd, group, shadow, gshadow;
- mkinitcpio.conf and mkinitcpio.conf.d/*, mkinitcpio.d/*.preset;
- kernel/cmdline, default/limine, /boot/limine.conf;
- modprobe.d/*, modules-load.d/*, vconsole.conf;
- pacman.conf and pacman.d/*;
- fstab, crypttab;
- network config, the firewall rules, sshd_config.

Users: removals and login shell changes get `!`.

The snapshot line gets `! no pre-snapshot` when snapper isn't set up.

The summary repeats every `!` line.

Tested with a category table, one fixture per category, plus a mutation check: removing a rule fails a test.

### 3.3 Secrets [B9, N5]

- Content derived from archix.secret() shows only as "secret changed" or "secret unchanged", computed by HMAC with a root-only key in /var/lib/archix. Nothing derived from a secret (no hash, no prefix) appears in diff output, --json, plan.json or the journal. Shadow hashes are never printed.
- A user diff that can't read the key says "secret: not checked without root" (exit 4).
- Leak sweep (reusing merge-tests' sweep): stdout, --json, plan.json and the journal contain no secret plaintext and no unkeyed hash (sha256, sha512, md5, any prefix) of any secret; the sweep also covers the generated flake dir and the closure of the built home store path.

### 3.4 Reviewed plans [Q1]

- `archix diff --save plan.json` writes the Plan along with a fingerprint covering:
  - config tree hash, lock hash;
  - the parsed pacman.conf, every sync db hash;
  - local db hash, managed file hashes;
  - the archix version;
  - the unchecked-item list.
- `archix switch --plan plan.json` recomputes. If anything differs, it refuses and names what moved ("repo cachyos-extra-znver4 changed since the plan; run diff again"). It also refuses if any item that was unchecked at diff time can't be checked now.
- A plain switch prints the diff and asks once. `--noconfirm` skips the question.

## 4. Journal and verification [B4, B5]

- The journal lives at /var/log/archix/switches/<n>/. It is outside the root snapshot on @log layouts. If /var/log is inside the root subvolume, diff warns that a rollback deletes the journal.
- <n> is the next number in that directory, so a rollback never reuses one.
- The journal contains:
  - plan.json (secrets as changed/unchanged only, section 3.3);
  - a copy of the config tree (tar), since a hash can't explain a past switch;
  - one record per step, fsynced before the step starts and after it ends;
  - the snapper pre/post numbers and the outcome.
- State that rolls back with /etc (hashes of the files archix last wrote) stays in /var/lib/archix.
- `archix log [n]` shows a switch. `archix log --verify n` checks only observations the executor doesn't produce:
  - (a) the [ALPM] lines in /var/log/pacman.log in the [ALPM] window from step 3 (lock taken) to step 8 (lock released) equal the plan's package lines exactly;
  - (b) `snapper status pre..post` [N4, N6]: every changed path under /etc, /usr and /opt must be owned by a planned package, a declared file, on the fixed daemon allowlist below, or match the write map entry of a hook pacman.log shows running or of a planned package's scriptlet.
  - the write map is fixed in this spec's appendix; it is a gate item: built from the hooks and scriptlets on the lived-in image and reviewed before any gate run counts, never extended because a switch failed: hook name or package → path globs it may write. Examples: systemd-sysusers → /etc/{passwd,group,shadow,gshadow}*; update-ca-trust → /etc/ssl/**, /etc/ca-certificates/**; systemd-hwdb → /etc/udev/hwdb.bin; dconf → /etc/dconf/db/**; glib schemas, gtk icon/gio/gdk-pixbuf/mime/gconv caches → their cache files under /usr; depmod → /usr/lib/modules/*/modules.*; locale-gen → /usr/lib/locale/**; cachyos-rate-mirrors → /etc/pacman.d/*mirrorlist*. A hook or scriptlet write outside its entry is unexplained; Under /var/lib only paths owned by planned packages or matching the write map are checked. Anything else in scope is "unexplained" and fails;
  - the daemon allowlist is fixed in this spec and printed by verify; changing it needs the user's review: /etc/adjtime, /etc/resolv.conf, /etc/NetworkManager/system-connections/*, /etc/machine-info, /etc/cups/*, /etc/ld.so.cache, /etc/pacman.d/gnupg/*, /etc/.updated, /etc/.pwd.lock, /etc/mtab;
  - (c) `systemctl is-enabled/is-active` for every unit the plan names;
  - (d) `flatpak list` matches the plan's flatpak group; the activated home-manager generation's store path equals the one built in step 2 (and recorded in the plan's fingerprint) [N7].
- Mutation tests: an executor that installs one extra package, writes one extra file under /etc, or writes under a planned package's /var/lib path outside its file list, or a hook that writes one unmapped file, must fail verify.
- Month criterion: verify passes for every switch.

## 5. Failure and rollback [B6, Q3]

- No automatic undo. A failed step stops the switch, and the journal records where it stopped.
- The message prints:
  - the pre snapshot number;
  - the layout's rollback procedure;
  - what the snapshot doesn't cover: /home, /var/log, /var/cache, /srv, /root, /tmp and the ESP.
- The rollback procedure is per layout. v1 has one: Limine + limine-snapper-sync with RESTORE_METHOD=replace:
  1. reboot;
  2. pick "Snapshots → <pre>" in the Limine menu;
  3. run `limine-snapper-restore`, restoring kernels too (`--kernels <ID>`), since the ESP isn't snapshotted.

  `snapper rollback` is never suggested where rootflags=subvol= is set.
- Before the first package or ESP write, the switch waits until limine-snapper-sync has written the pre snapshot's boot entry. It checks that limine.conf has it, with a timeout that refuses.
- archix's pre/post pairs get userdata important=yes, so number cleanup doesn't eat them.
- Snapper not set up: the switch needs `--no-snapshot`, which is a flag, not a refusal.
- Failure injection, a VM test per step:
  - pacman refusal;
  - AUR build failure;
  - a file write error;
  - a unit that won't start;
  - home failure;
  - flatpak failure.

  Each case must give the message and the journal and leave pacman usable. A stale db.lck gets one exact instruction. Each case must converge on `archix switch` again, with diff exiting 0 after.
- A VM test follows the printed rollback instructions literally, then asserts that pacman -Q, uname -r and /usr/lib/modules match the pre state.

## 6. Drift

- No strict mode, no hooks.
- At switch: undeclared explicit packages are removed (with `remove_undeclared = false` this is additive), declared missing ones are installed, and orphans go as `-Rs`. Each is a diff line with a drift reason.
- Managed files differing from what archix last wrote are restored, and the hand edit is kept as `.archix-save`. Unmanaged files are never touched.
- Users: archix removes only users it created, never deletes a home, and the line is `!`.

## 7. Home (user decision 2026-11-16: Lua→home-manager stays in v1)

One per user: archix's built-in home modules, or `home.<user>.home_manager = { ... }` with home-manager option names.

- archix generates ~/.config/archix/home-manager/<user>/{home.nix,flake.nix,flake.lock}; the home-manager and nixpkgs revs come from archix's lock.
- Lua→Nix mapping [N8]:
  - strings are emitted as `"..."` with `\`, `"` and `${` escaped (`\\`, `\"`, `\${`), control characters as `\n`, `\t`, `\r`, everything else verbatim UTF-8;
  - attribute names that aren't Nix identifiers are quoted the same way;
  - Lua integers → Nix ints, Lua floats → Nix floats (1.0 stays `1.0`); NaN and ±inf are errors, as is NUL in a string;
  - a non-empty table with keys 1..n → list, string keys → attrset, mixed keys → error; an empty table is an error, written `hm.list{}` or `hm.attrs{}`;
  - `hm.null` → `null`; `hm.pkg "ripgrep"` → `pkgs.ripgrep`; `hm.raw "<nix expr>"` verbatim for what Lua can't say (mkForce, functions, lib calls);
  - file sources are explicit [N11]: `hm.path "hypr/hyprland.conf"` (relative to the config tree) → `./files/hypr/hyprland.conf`, copied into the generated flake dir; a plain string always stays a string. A hm.path that escapes the tree, or a symlink inside it pointing outside, is an error. Directories copy recursively and keep the executable bit. Copied files are part of the plan fingerprint;
  - secrets never reach home-manager [N10]: archix.secret() values carry a taint through eval; the generator refuses any tainted value (exit 2, naming the option path) with "in v1 secrets go through a system file with a restrictive mode or the built-in modules, never home-manager, because /nix/store is world-readable". Every file a hm.path copies is checked against the secret list too;
  - the generated flake dir is written as the user, not root;
  - test: round-trip property test, Lua value → generated Nix → `nix eval --json` == the Lua value as JSON, over random strings containing `${`, `\`, `"`, `''`, newlines and unicode, plus empty tables, int/float, null, odd attribute names and hm.path cases (file, directory with an executable, escaping path, outward symlink).
- `home-manager build` runs at switch step 2 with no lock; unknown option names or evaluation errors refuse there, before anything system-side changes [N7]. The switch activates exactly that store path.
- diff shows the generated home.nix diff, then runs `home-manager build` as the user into a temporary gcroot (writes only /nix/store, a cache; needs the network) and lists the home files that would change by comparing the new generation's home-files with the current one. `diff --no-build` skips that and exits 4.
- The switch activates the step-2 store path as that user after the system steps (step 9), with the user's manager running. If that needs linger, linger is declared (`home.<user>.linger = true`) and shown in diff; archix never enables it as a side effect.
- Activation scripts are code outside the plan contract; diff says so on the home line. Failures follow section 5; home-manager's own generations are the rollback for /home.
- Nix and home-manager are declared packages; diff marks the first switch that installs nix as `!`. That switch can't build home beforehand, so it defers the home group, and diff's home line says "builds after nix is installed; home applies on the next switch".

## 8. init

- `archix init` writes the minimal config for the machine as it stands: explicit packages, enabled services, users, the pinning mode and lock. diff right after must exit 0.
- `archix init --agent-prompt` prints docs/migrate-prompt.md. Its rules: migrations only add; any `remove` line in a migration diff stops the migration, and the agent asks.

## 9. Removal list

Each removal is a separate commit. Its message lists the tests for each kept module before and after. A kept module whose only coverage was through a removed path gets a new test in the same commit.

Removed:
- snapshot/layout/keep;
- /.archix and @archix/@varlib handling;
- config.git (replaced by the journal's config copy);
- rescue, emergency, eject;
- boot, menu, limine, grub, esp, initrd, authenticode, dkms signing, health/boot counting;
- pacman hooks and the pacman_hook command;
- displaced swap, conflict takeover, repack, soname machinery;
- generations, revert;
- holds, strict;
- capture (dotfile scraping) and the init TUI; captured.rs stays only for evaluating the config as its owner in an isolated uid, the one evaluation path root uses for both diff and switch;
- uninstall;
- old_model;
- archix-static packaging, its .sig, release-key publishing;
- the old VM suites (boot, powercut, rescue, replay varlib, records) and their release-gate entries.

Kept:
- eval and options;
- lock/archive (record), aur fetch/srcinfo/review;
- system/apply, accounts, credentials, secrets, fstab (as files), firewall, flatpak;
- plan/plan_diff, rebuilt around Plan.

Docs and site:
- the banner goes up first ("being rescoped, don't install");
- the 26 pages and landing claims get rewritten after the month;
- archix-git keeps building main (user decision 2026-11-16); the gate's recorded pass is on the commit the user installs.

## 10. Gates before the month

Recorded-pass lines are on the exact commit the user installs, with archix-git built from that commit. The suites:
- Images:
  - stock CachyOS (installer, Limine, snapper);
  - stock archinstall (systemd-boot, no snapper), which covers `--no-snapshot`. That rollback procedure is "none", and diff says so;
  - a lived-in image built from package lists, with no personal data. It has several hundred explicit and a dozen or more foreign packages including paru and yay, two kernels, nvidia-open-dkms plus headers, flatpaks and hand-edited /etc. On it, init followed by diff exits 0.
- Section 3/4 checks:
  - fidelity per plan group: diff --json → switch → verify → diff exits 0;
  - the mutation checks (B4) and the risk-marker table;
  - the concurrency test, non-root vs root diff, the secrets sweep;
  - the drift test: `pacman -Syu`, a hand install and a hand edit between switches.
- Section 5 checks: failure injection per step, and the printed rollback followed literally.
- Power cut: the list of Arch-level risk windows is frozen before the run (mkinitcpio writing the initramfs on the FAT ESP, limine-snapper-sync rewriting limine.conf); those become documented risk windows on the site. A cut point outside that list that needs a live USB is a fail. A cut mid-transaction: the expected outcome is recovery by `archix switch` again, which reinstalls only the interrupted transaction's packages with an overwrite limited to those packages' own file lists (from their .MTREE), never a blanket --overwrite; the alternative is the snapper rollback. Written per cut point before running.
- Uninstall: remove archix; pacman -Syu, paru, snapper and limine-snapper-sync still work; the leftovers are as listed in section 1.

The month: the critic's criteria, amended so that verify (section 4) passes for every switch. Approved by the user 2026-11-16.

## 11. Open questions for the user

1. Spec location in version control.
