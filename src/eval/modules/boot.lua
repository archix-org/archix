local t = archix.types

return {
  options = {
    boot = {
      loader = archix.option {
        type = t.nullable(t.enum { "systemd-boot", "grub", "limine" }),
        default = archix.null,
        description = "The boot loader archix writes entries for. systemd-boot 260 or newer gets an entry for the current system and for snapshots, with boot counting and automatic fallback; GRUB gets the same entries in its menu, picked by hand; on Limine archix writes no entries and leaves limine.conf to limine-snapper-sync, so a fallback is a manual pick there.",
      },
      esp = archix.option {
        type = t.path,
        default = "/efi",
        description = "Where the EFI system partition is mounted.",
      },
      kernel = archix.option { type = t.str, default = "linux", description = "The kernel package boot entries use." },
      microcode = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "Microcode images in /boot loaded before the initramfs, such as amd-ucode.img.",
      },
      tries = archix.option {
        type = t.int,
        default = 2,
        description = "How many boots systemd-boot tries the current system's entry after each change before it falls back, first to the snapshot of how the system last booted fine, booted read-only in try mode; a boot that reaches boot-complete.target and passes the health check counts as good. Needs systemd-boot 260 or newer on the ESP; 0 turns boot counting off.",
      },
      unblessed_timeout = archix.option {
        type = t.int,
        default = 10,
        description = "Minutes a boot systemd-boot is still counting may take to count as good, or sit in emergency or rescue mode, before it reboots itself so the fallback can start with nobody at the machine; nobody may be logged in. 0 turns it off.",
      },
      secure_boot = archix.option {
        type = t.nullable(t.bool),
        default = archix.null,
        description = "Sign boot entries with sbctl; by default, whenever sbctl has keys.",
      },
      extra_entries = archix.option {
        type = t.attrs_of(t.lines),
        default = {},
        description = "More systemd-boot entries archix writes to the ESP's loader/entries, keyed by file name, such as [\"memtest86+.conf\"] = \"title Memtest86+\\nefi /memtest86+/memtest.efi\". With Secure Boot on, whatever they start must be signed too.",
      },
      protected_entry = archix.option {
        type = t.bool,
        default = false,
        description = "Also write an 'archix protected' entry for the current system with the kernel command line Easy Anti-Cheat Linux v2 asks for (integrity lockdown, kexec off, the BPF LSM, IOMMU without passthrough). It can't make the kernel well-known-signed; `archix secure-boot check --anticheat` shows what's still missing.",
      },
      rescue_entry = archix.option {
        type = t.bool,
        default = true,
        description = "Keep an 'archix rescue' entry in the boot menu, after the others and never the default: the current kernel with an initramfs of its own that mounts nothing of the layout and opens `archix rescue`, to check the root and the ESP, reset a password, open a shell or, with rollbacks on btrfs, roll back to a snapshot (`archix rollback N`), without a live USB. Refreshed after each switch, signed when Secure Boot signing is on. Before its prompt or a shell it asks for the password of root or of a wheel (or sudo) user from the root's /etc/shadow, three tries per boot, and on an encrypted root for the LUKS passphrase first, never unlocking with a TPM; with root locked and no wheel password it opens nothing. Under Secure Boot its kernel command line is part of the signed image and can't be edited; without Secure Boot anyone at the console can edit it, as they could boot a live USB.",
      },
      health_check = {
        default_target = archix.option {
          type = t.bool,
          default = true,
          description = "The boot must reach its default target before it counts as good.",
        },
        failed_units = archix.option {
          type = t.bool,
          default = true,
          description = "No unit may have failed that hadn't already failed at the last boot that counted as good.",
        },
        display_manager = archix.option {
          type = t.bool,
          default = true,
          description = "When a display manager is enabled (display-manager.service), it must be active.",
        },
        units = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "More systemd units that must be active, such as \"sshd.service\", before a boot counts as good. A target that still fails after boot.health_check.timeout marks the entry bad, so the boot after falls back, and the login notes and `archix status` say which target failed and when.",
        },
        listening = archix.option {
          type = t.list_of(t.int),
          default = {},
          description = "TCP ports something on this machine must listen on, such as 22 for sshd, before a boot counts as good.",
        },
        reachable = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "Hosts that must answer before a boot counts as good: \"host:port\" must accept a TCP connection, a bare host must answer a ping, and \"gateway\" is the default IPv4 gateway. A host that's down for a reason outside the deployment (the router, a captive portal, a laptop on a train) fails the check just the same and falls back from a good update, so list only what is always up when this machine boots; switching again once it answers brings the update back.",
        },
        login = archix.option {
          type = t.bool,
          default = false,
          description = "Each user archix declares must be able to log in: `su -s <shell> <user> -c true` must succeed. Without it, a boot still doesn't count as good while a declared user's login shell is missing or not in /etc/shells.",
        },
        timeout = archix.option {
          type = t.int,
          default = 120,
          description = "Seconds the health check keeps retrying its targets after boot before it fails.",
        },
        reboot_on_failure = archix.option {
          type = t.bool,
          default = false,
          description = "Reboot as soon as the health check fails, so the fallback starts at once; otherwise boot.unblessed_timeout reboots it once nobody is logged in.",
        },
      },
      revoke_unused = archix.option {
        type = t.bool,
        default = false,
        description = "Experimental, tested on OVMF only: after every switch, add the boot images archix signed and no entry uses any more to dbx, as `archix secure-boot revoke` does, so the firmware refuses them. Needs Secure Boot keys from sbctl as files, and declares efitools.",
      },
    },
  },
}
