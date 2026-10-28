local t = archix.types

return {
  options = {
    file_systems = archix.option {
      type = t.attrs_of(t.submodule {
        device = archix.option {
          type = t.str,
          description = "What to mount: \"UUID=…\", \"LABEL=…\", \"PARTUUID=…\", a /dev path, the directory to bind with options = { \"bind\" }, \"tmpfs\", or a network share such as \"server:/export\".",
        },
        fs_type = archix.option {
          type = t.str,
          default = "auto",
          description = "The filesystem type, such as ext4, btrfs, vfat, tmpfs or nfs; a bind mount's is written as none.",
        },
        options = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "Mount options, such as { \"noatime\", \"subvol=@data\" } or { \"bind\" }; none means defaults.",
        },
        check = archix.option {
          type = t.bool,
          default = false,
          description = "fsck the filesystem at boot before mounting it (fs_passno 2).",
        },
        required = archix.option {
          type = t.nullable(t.bool),
          default = archix.null,
          description = "Make the boot wait for it and stop in emergency mode without it. By default mounts at /home, /var and /srv and under them are required, as a boot without one would show an empty directory on the root filesystem that programs fill with fresh files, hidden once the mount is back; elsewhere a mount is nofail, its device waited for 30 seconds (x-systemd.device-timeout=30s, unless options set one), so an unplugged or dead disk leaves it unmounted instead of stopping every boot. false makes a mount under /home, /var or /srv nofail too.",
        },
        encrypted = archix.option {
          type = t.nullable(t.submodule {
            device = archix.option { type = t.str, description = "The LUKS device, such as \"UUID=…\" of the encrypted partition." },
            key_file = archix.option {
              type = t.nullable(t.str),
              default = archix.null,
              description = "A key file to unlock it with; without one, systemd asks for the passphrase at boot.",
            },
            options = archix.option { type = t.list_of(t.str), default = {}, description = "crypttab options, such as { \"discard\" }." },
          }),
          default = archix.null,
          description = "Unlock device at boot through an /etc/crypttab line archix writes, named after the /dev/mapper/<name> this mount's device must be.",
        },
      }),
      default = {},
      description = "Filesystems to mount at boot, keyed by mount point, written into /etc/fstab, which archix writes whole in each deployment: its own lines for the root and the ESP, then these and swap. Like the rest of a deployment they apply at the next boot, and a rollback brings back the fstab of the deployment rolled back to. /, /usr, /opt, /boot, /etc, /var, /var/lib, /var/lib/pacman, /var/lib/dkms, /archix and the ESP can't be declared; /usr/local can.",
    },
    drop_mounts = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Mount points, and /dev/mapper/<name> for /etc/crypttab volumes, whose lines the next deployment may leave out of /etc/fstab and /etc/crypttab although nothing declares them. Without this a switch refuses to drop a line that's mounted or open now, or for /home, /var or /srv and under them, naming each, so a hand-added line can't vanish with its data. Take an entry out once the deployment without it has booted.",
    },
    swap = archix.option {
      type = t.list_of(t.submodule {
        device = archix.option { type = t.nullable(t.str), default = archix.null, description = "A swap partition, such as \"UUID=…\" or a /dev path." },
        file = archix.option { type = t.nullable(t.path), default = archix.null, description = "A swap file, such as /swap/swapfile." },
        size = archix.option {
          type = t.nullable(t.str),
          default = archix.null,
          description = "For a file: its size, such as \"8G\" or \"512M\"; archix-swapfile creates or resizes it at boot before swapon. On btrfs it's made with `btrfs filesystem mkswapfile` in a subvolume of its own, which archix creates when the file's directory is missing, as btrfs can't snapshot a subvolume holding an active swap file. Without a size the file must exist.",
        },
        priority = archix.option { type = t.nullable(t.int), default = archix.null, description = "The swap priority (pri=)." },
        options = archix.option { type = t.list_of(t.str), default = {}, description = "More swap options, such as { \"discard\" }." },
        required = archix.option { type = t.bool, default = false, description = "Make the boot wait for it; by default it's nofail." },
      }),
      default = {},
      description = "Swap partitions and files, written into each deployment's /etc/fstab; they apply at the next boot.",
    },
    zram = {
      enable = archix.option {
        type = t.bool,
        default = false,
        description = "Swap on compressed RAM at /dev/zram0 through zram-generator, which archix declares; written to /etc/systemd/zram-generator.conf and set up at the next boot.",
      },
      size = archix.option {
        type = t.any,
        default = archix.null,
        description = "zram-size: an expression in ram (MiB), such as \"ram / 2\" or \"min(ram / 2, 4096)\", or a number of MiB; by default zram-generator's, min(ram / 2, 4096).",
      },
      algorithm = archix.option { type = t.nullable(t.str), default = archix.null, description = "compression-algorithm, such as zstd or lz4." },
      priority = archix.option { type = t.nullable(t.int), default = archix.null, description = "swap-priority; zram-generator's default is 100." },
    },
  },
  config = function(cfg)
    if cfg.zram.enable then return { packages = { "zram-generator" } } end
    return {}
  end,
}
