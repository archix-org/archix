//! The write map and daemon allowlist for `archix log --verify` (SCOPE 4): a gate item, reviewed by the user and the critic before any gate run counts, and never extended because a switch failed.

/// One write map entry: a hook as pacman.log names it, or `scriptlet:<pkg>`, and the path globs it may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub source: &'static str,
    pub globs: &'static [&'static str],
}

const fn e(source: &'static str, globs: &'static [&'static str]) -> Entry {
    Entry { source, globs }
}

const ACCOUNTS: &[&str] = &["/etc/{passwd,group,shadow,gshadow}*"];
const CA_TRUST: &[&str] = &["/etc/ssl/**", "/etc/ca-certificates/**"];
const LDCONFIG: &[&str] = &["/etc/ld.so.cache"];
const GCONV: &[&str] = &["/usr/lib/gconv/gconv-modules.cache"];
const GCONV32: &[&str] = &["/usr/lib32/gconv/gconv-modules.cache"];
const LOCALE: &[&str] = &["/usr/lib/locale/**"];
const MIME: &[&str] = &[
    "/usr/share/mime/{XMLnamespaces,aliases,generic-icons,globs,globs2,icons,magic,mime.cache,subclasses,treemagic,types,version}",
    "/usr/share/mime/{application,audio,chemical,font,image,inode,message,model,multipart,text,video,x-content,x-epoc,x-schema-handler}/**",
];
const INITRAMFS: &[&str] = &["/boot/**", "/efi/**", "/var/lib/limine/**"];
const DKMS: &[&str] = &[
    "/usr/lib/modules/*/updates/**",
    "/usr/lib/modules/*/modules.*",
    "/var/lib/dkms/**",
];
const PIXBUF: &[&str] = &["/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"];
const GIO: &[&str] = &["/usr/lib/gio/modules/giomodule.cache"];
const GIO32: &[&str] = &["/usr/lib32/gio/modules/giomodule.cache"];
const SCHEMAS: &[&str] = &["/usr/share/glib-2.0/schemas/gschemas.compiled"];
const IMMODULES: &[&str] = &["/usr/lib/gtk-3.0/3.0.0/immodules.cache"];
const ICONS: &[&str] = &["/usr/share/icons/*/icon-theme.cache"];
const DESKTOP_DB: &[&str] = &["/usr/share/applications/mimeinfo.cache"];
const SHELLS: &[&str] = &["/etc/shells"];
const KEYRING: &[&str] = &["/etc/pacman.d/gnupg/**"];
const BRANDING: &[&str] = &[
    "/etc/lsb-release",
    "/etc/os-release",
    "/etc/issue",
    "/usr/share/factory/etc/issue",
];
const USER_SOCKETS: &[&str] = &["/etc/systemd/user/sockets.target.wants/*"];
const TMPFILES: &[&str] = &[
    "/etc/{arch-release,crypttab,fstab,host.conf,hosts,issue,ld.so.conf,libaudit.conf,locale.conf,mtab,nsswitch.conf,os-release,profile,securetty,shells,subgid,subuid,vconsole.conf}",
    "/etc/{passwd,group,shadow,gshadow}",
    "/etc/{pam.d,profile.d,ssh,audit,audisp,credstore,credstore.encrypted,tpm2-tss,issue.d,motd.d,valkey,openrgb}/**",
    "/etc/polkit-1/rules.d",
    "/etc/openvpn/{client,server}",
    "/var/lib/{alsa,dbus,lastlog,libuuid,private,rpcbind,sddm,plasmalogin,postgres,tor,valkey,tpm2-tss,plocate,openvpn}/**",
    "/var/lib/systemd/{coredump,ephemeral-trees,pstore,network}",
];

/// The write map, sorted by source; seeded from the hooks and scriptlets on the dev machine and SCOPE 4's examples.
pub const WRITE_MAP: &[Entry] = &[
    e("05-snap-pac-pre.hook", &[]),
    e("10-glibc-locale-gen.hook", LOCALE),
    e(
        "10-glibc-remove-locale-archive.hook",
        &["/usr/lib/locale/locale-archive"],
    ),
    e("10-limine-snapper-lock.hook", &[]),
    e("10-openssh-mark-sshd-for-restart.hook", &[]),
    e("10-snap-pac-removal.hook", &[]),
    e("11-glibc-ldconfig.hook", LDCONFIG),
    e("11-glibc-remove-ldconfig-cache.hook", LDCONFIG),
    e("12-glibc-iconvconfig.hook", GCONV),
    e("12-glibc-remove-iconvconfig-cache.hook", GCONV),
    e("12-lib32-glibc-iconvconfig.hook", GCONV32),
    e("12-lib32-glibc-remove-iconvconfig-cache.hook", GCONV32),
    e("20-systemd-sysusers.hook", ACCOUNTS),
    e("21-systemd-tmpfiles.hook", TMPFILES),
    e("25-systemd-binfmt.hook", &[]),
    e("25-systemd-catalog.hook", &["/var/lib/systemd/catalog/database"]),
    e(
        "25-systemd-hwdb.hook",
        &["/etc/udev/hwdb.bin", "/usr/lib/udev/hwdb.bin"],
    ),
    e("25-systemd-sysctl.hook", &[]),
    e("30-systemd-daemon-reload-system.hook", &[]),
    e("30-systemd-daemon-reload-user.hook", &[]),
    e("30-update-mime-database.hook", MIME),
    e("35-systemd-enqueue-marked.hook", &[]),
    e("35-systemd-udev-reload.hook", &[]),
    e("35-systemd-update.hook", &["/usr"]),
    e("36-systemd-modules-load.hook", &[]),
    e("40-fontconfig-config.hook", &["/etc/fonts/conf.d/*"]),
    e("40-update-ca-trust.hook", CA_TRUST),
    e("60-depmod.hook", &["/usr/lib/modules/*/modules.*"]),
    e("60-limine-mkinitcpio-remove-pre.hook", INITRAMFS),
    e("60-mkinitcpio-remove.hook", INITRAMFS),
    e("70-dkms-install.hook", DKMS),
    e("70-dkms-upgrade.hook", DKMS),
    e("70-openssh-restart-sshd.hook", &[]),
    e("71-dkms-remove.hook", DKMS),
    e("80-limine-efi-deploy.hook", &["/boot/**", "/efi/**"]),
    e(
        "90-desktop-caps.hook",
        &["/usr/bin/gamescope", "/usr/bin/gsr-kms-server"],
    ),
    e("90-limine-mkinitcpio-remove-post.hook", INITRAMFS),
    e("90-mkinitcpio-install.hook", INITRAMFS),
    e("90-syslinux.hook", &["/boot/**"]),
    e("90-update-appstream-cache.hook", &["/var/cache/swcatalog/**"]),
    e("accounts-daemon-restart.hook", &[]),
    e("cachyos-branding.hook", BRANDING),
    e(
        "cachyos-plymouth-initramfs.hook",
        &["/usr/lib/plymouth/plymouth-update-initrd"],
    ),
    e(
        "cachyos-rate-mirrors.hook",
        &[
            "/etc/systemd/system/timers.target.wants/reflector.timer",
            "/etc/pacman.d/*mirrorlist*",
        ],
    ),
    e("cachyos-reboot-required.hook", &[]),
    e("dbus-reload.hook", &[]),
    e("dconf-update.hook", &["/etc/dconf/db/**"]),
    e("detect-old-perl-modules.hook", &[]),
    e("fontconfig-32.hook", &["/var/cache/fontconfig/**"]),
    e("fontconfig.hook", &["/var/cache/fontconfig/**"]),
    e("gdk-pixbuf-query-loaders.hook", PIXBUF),
    e("gdk-pixbuf-remove-loaders-cache.hook", PIXBUF),
    e("gio-querymodules-32.hook", GIO32),
    e("gio-querymodules.hook", GIO),
    e("gio-remove-module-cache-32.hook", GIO32),
    e("gio-remove-module-cache.hook", GIO),
    e("glib-compile-schemas.hook", SCHEMAS),
    e("glib-remove-compiled-schemas.hook", SCHEMAS),
    e("gtk-query-immodules-3.0.hook", IMMODULES),
    e("gtk-remove-immodules-cache.hook", IMMODULES),
    e("gtk-update-icon-cache.hook", ICONS),
    e("gtk4-querymodules.hook", &["/usr/lib/gtk-4.0/4.0.0/*/giomodule.cache"]),
    e("gvfsd.hook", &[]),
    e("lsb-release.hook", BRANDING),
    e(
        "luarocks-make-manifest-5.1.hook",
        &["/usr/lib/luarocks/rocks-5.1/manifest*"],
    ),
    e(
        "luarocks-make-manifest-5.2.hook",
        &["/usr/lib/luarocks/rocks-5.2/manifest*"],
    ),
    e(
        "luarocks-make-manifest-5.3.hook",
        &["/usr/lib/luarocks/rocks-5.3/manifest*"],
    ),
    e(
        "luarocks-make-manifest-5.4.hook",
        &["/usr/lib/luarocks/rocks-5.4/manifest*"],
    ),
    e(
        "luarocks-make-manifest-5.5.hook",
        &["/usr/lib/luarocks/rocks-5.5/manifest*"],
    ),
    e("man-db-remove-cache.hook", &["/var/cache/man/**"]),
    e("nvimdoc.hook", &["/usr/share/nvim/site/pack/dist/start/*/doc/tags*"]),
    e("os-release.hook", BRANDING),
    e("rebuild-detector.hook", &[]),
    e("shared-mime-info-remove-cache.hook", MIME),
    e("texinfo-install.hook", &["/usr/share/info/dir"]),
    e("texinfo-remove.hook", &["/usr/share/info/dir"]),
    e("update-desktop-database.hook", DESKTOP_DB),
    e("update-vlc-plugin-cache.hook", &["/usr/lib/vlc/plugins/plugins.dat"]),
    e("vimdoc.hook", &["/usr/share/vim/vimfiles/doc/tags*"]),
    e("xorg-mkfontscale.hook", &["/usr/share/fonts/*/fonts.{scale,dir}"]),
    e(
        "zz-sbctl.hook",
        &["/boot/**", "/efi/**", "/usr/lib/modules/*/vmlinuz", "/var/lib/sbctl/**"],
    ),
    e("zz-snap-pac-post.hook", &[]),
    e("scriptlet:archlinux-keyring", KEYRING),
    e("scriptlet:bash", SHELLS),
    e("scriptlet:ca-certificates-utils", CA_TRUST),
    e("scriptlet:cachyos-keyring", KEYRING),
    e(
        "scriptlet:cachyos-rate-mirrors",
        &["/etc/systemd/system/timers.target.wants/cachyos-rate-mirrors.timer"],
    ),
    e(
        "scriptlet:cachyos-settings",
        &["/etc/X11/xorg.conf.d/00-keyboard.conf", "/etc/systemd/system/*.wants/*"],
    ),
    e(
        "scriptlet:cachyos-snapper-support",
        &["/etc/systemd/system/timers.target.wants/snapper-cleanup.timer"],
    ),
    e("scriptlet:cups", &["/etc/cups/**"]),
    e("scriptlet:fish", SHELLS),
    e("scriptlet:flutter-bin", ACCOUNTS),
    e("scriptlet:git", SHELLS),
    e("scriptlet:gnupg", USER_SOCKETS),
    e("scriptlet:jdk-openjdk", CA_TRUST),
    e("scriptlet:jdk-temurin", CA_TRUST),
    e("scriptlet:kiri-lite-bin", ACCOUNTS),
    e("scriptlet:mono", &["/usr/share/.mono/**"]),
    e("scriptlet:nushell", SHELLS),
    e("scriptlet:p11-kit", USER_SOCKETS),
    e(
        "scriptlet:systemd",
        &[
            "/etc/{passwd,group,shadow,gshadow}*",
            "/etc/shells",
            "/etc/machine-id",
            "/etc/systemd/system/*.wants/*",
            "/etc/systemd/user/*.wants/*",
        ],
    ),
    e("scriptlet:zsh", SHELLS),
];

/// SCOPE 4's daemon allowlist, verbatim; changing it needs the user's review.
pub const DAEMON_ALLOWLIST: &[&str] = &[
    "/etc/adjtime",
    "/etc/resolv.conf",
    "/etc/NetworkManager/system-connections/*",
    "/etc/machine-info",
    "/etc/cups/*",
    "/etc/ld.so.cache",
    "/etc/pacman.d/gnupg/*",
    "/etc/.updated",
    "/etc/.pwd.lock",
    "/etc/mtab",
];

/// Paths under /var/lib verify never checks: pacman's own db, which every transaction rewrites; needs the user's review like the allowlist.
pub const UNCHECKED: &[&str] = &["/var/lib/pacman/**"];

pub fn entry(source: &str) -> Option<&'static Entry> {
    WRITE_MAP.iter().find(|entry| entry.source == source)
}

/// The sources whose globs match `path`.
pub fn sources_matching(path: &str) -> Vec<&'static str> {
    WRITE_MAP
        .iter()
        .filter(|entry| entry.globs.iter().any(|glob| matches(glob, path)))
        .map(|entry| entry.source)
        .collect()
}

pub fn allowlisted(path: &str) -> bool {
    DAEMON_ALLOWLIST.iter().any(|glob| matches(glob, path))
}

/// Shell-style glob over absolute paths: `*` and `?` stay within a segment, `**` spans any number of segments, `{a,b}` alternates.
pub fn matches(glob: &str, path: &str) -> bool {
    expand_braces(glob).iter().any(|pattern| {
        let pattern: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match_segments(&pattern, &path)
    })
}

fn expand_braces(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_string()];
    };
    let Some(close) = glob[open..].find('}').map(|i| open + i) else {
        return vec![glob.to_string()];
    };
    let (head, tail) = (&glob[..open], &glob[close + 1..]);
    glob[open + 1..close]
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{head}{alt}{tail}")))
        .collect()
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => path.split_first().is_some_and(|(seg, path_rest)| {
            match_segment(first.as_bytes(), seg.as_bytes()) && match_segments(rest, path_rest)
        }),
    }
}

fn match_segment(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| match_segment(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && match_segment(rest, &text[1..]),
        Some((c, rest)) => text.first() == Some(c) && match_segment(rest, &text[1..]),
    }
}

/// `--print-allowlist`: the daemon allowlist and the write map, as reviewed.
pub fn render() -> String {
    let mut out = String::from("daemon allowlist (SCOPE 4; changing it needs the user's review):\n");
    for glob in DAEMON_ALLOWLIST {
        out.push_str(&format!("  {glob}\n"));
    }
    out.push_str("never checked under /var/lib:\n");
    for glob in UNCHECKED {
        out.push_str(&format!("  {glob}\n"));
    }
    out.push_str("write map (a gate item, reviewed by the user and the critic before any gate run counts):\n");
    for entry in WRITE_MAP {
        let globs = if entry.globs.is_empty() {
            "(nothing)".to_string()
        } else {
            entry.globs.join(" ")
        };
        out.push_str(&format!("  {:<44} {}\n", entry.source, globs));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_follow_shell_rules_per_segment() {
        assert!(matches("/etc/{passwd,group,shadow,gshadow}*", "/etc/gshadow-"));
        assert!(matches("/etc/{passwd,group,shadow,gshadow}*", "/etc/passwd"));
        assert!(!matches("/etc/{passwd,group,shadow,gshadow}*", "/etc/hosts"));
        assert!(matches(
            "/usr/lib/modules/*/modules.*",
            "/usr/lib/modules/6.17.1-arch1-1/modules.dep.bin"
        ));
        assert!(!matches(
            "/usr/lib/modules/*/modules.*",
            "/usr/lib/modules/6.17.1/kernel/modules.x"
        ));
        assert!(matches("/etc/ssl/**", "/etc/ssl/certs/ca-certificates.crt"));
        assert!(matches("/etc/ssl/**", "/etc/ssl"));
        assert!(!matches("/etc/ssl/**", "/etc/ssl2/x"));
        assert!(matches("/etc/cups/*", "/etc/cups/printers.conf"));
        assert!(!matches("/etc/cups/*", "/etc/cups/ppd/x.ppd"));
        assert!(matches(
            "/usr/share/fonts/*/fonts.{scale,dir}",
            "/usr/share/fonts/TTF/fonts.dir"
        ));
        assert!(matches(
            "/usr/lib/luarocks/rocks-5.?/manifest*",
            "/usr/lib/luarocks/rocks-5.4/manifest-5.4"
        ));
    }

    #[test]
    fn the_table_is_sorted_unique_and_absolute() {
        let sources: Vec<&str> = WRITE_MAP.iter().map(|e| e.source).collect();
        let hooks: Vec<&str> = sources
            .iter()
            .copied()
            .filter(|s| !s.starts_with("scriptlet:"))
            .collect();
        let scriptlets: Vec<&str> = sources
            .iter()
            .copied()
            .filter(|s| s.starts_with("scriptlet:"))
            .collect();
        for list in [&hooks, &scriptlets] {
            assert!(list.windows(2).all(|w| w[0] < w[1]), "unsorted or duplicate: {list:?}");
        }
        assert_eq!(hooks.len() + scriptlets.len(), sources.len());
        assert!(hooks.iter().all(|h| h.ends_with(".hook")));
        for entry in WRITE_MAP {
            assert!(entry.globs.iter().all(|g| g.starts_with('/')), "{}", entry.source);
        }
    }

    #[test]
    fn scope_examples_are_mapped() {
        let maps = |source: &str, path: &str| entry(source).is_some_and(|e| e.globs.iter().any(|g| matches(g, path)));
        assert!(maps("20-systemd-sysusers.hook", "/etc/shadow-"));
        assert!(maps(
            "40-update-ca-trust.hook",
            "/etc/ca-certificates/extracted/tls-ca-bundle.pem"
        ));
        assert!(maps("25-systemd-hwdb.hook", "/etc/udev/hwdb.bin"));
        assert!(maps("dconf-update.hook", "/etc/dconf/db/local"));
        assert!(maps(
            "glib-compile-schemas.hook",
            "/usr/share/glib-2.0/schemas/gschemas.compiled"
        ));
        assert!(maps(
            "gtk-update-icon-cache.hook",
            "/usr/share/icons/hicolor/icon-theme.cache"
        ));
        assert!(maps("60-depmod.hook", "/usr/lib/modules/6.17.1/modules.alias"));
        assert!(maps("10-glibc-locale-gen.hook", "/usr/lib/locale/locale-archive"));
        assert!(maps("cachyos-rate-mirrors.hook", "/etc/pacman.d/cachyos-mirrorlist"));
        assert!(maps("90-mkinitcpio-install.hook", "/boot/initramfs-linux.img"));
        assert!(maps("11-glibc-ldconfig.hook", "/etc/ld.so.cache"));
        assert!(entry("30-systemd-daemon-reload-system.hook").is_some_and(|e| e.globs.is_empty()));
        assert!(allowlisted("/etc/pacman.d/gnupg/trustdb.gpg"));
        assert!(!allowlisted("/etc/pacman.d/mirrorlist"));
    }

    #[test]
    fn print_lists_both_tables() {
        let text = render();
        assert!(text.contains("/etc/adjtime"));
        assert!(text.contains("21-systemd-tmpfiles.hook"));
        assert!(text.contains("gate item"));
    }
}
