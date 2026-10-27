local t = archix.types

return {
  options = {
    boot = {
      loader = archix.option {
        type = t.nullable(t.enum { "systemd-boot" }),
        default = archix.null,
        description = "The boot loader archix writes each deployment's boot entry for; a switch needs it, as every generation boots as its own deployment.",
      },
      esp = archix.option {
        type = t.path,
        default = "/efi",
        description = "Where the EFI system partition is mounted; not /boot, which each deployment keeps its own copy of.",
      },
      kernel = archix.option { type = t.str, default = "linux", description = "The kernel package boot entries use." },
      microcode = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "Microcode images in /boot loaded before the initramfs, such as amd-ucode.img.",
      },
      tries = archix.option {
        type = t.int,
        default = 3,
        description = "How many boots systemd-boot tries a new deployment's entry before it falls back to the last deployment that booted, which archix then makes current; a boot that reaches boot-complete.target counts as good. Needs systemd-boot 260 or newer on the ESP; 0 turns boot counting off.",
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
        description = "Also write an 'archix protected' entry for each deployment with the kernel command line Easy Anti-Cheat Linux v2 asks for (integrity lockdown, kexec off, the BPF LSM, IOMMU without passthrough). It can't make the kernel well-known-signed; `archix secure-boot check --anticheat` shows what's still missing.",
      },
      rescue_entry = archix.option {
        type = t.bool,
        default = true,
        description = "Keep an 'archix rescue' entry in the boot menu, after the deployments and never the default: the kernel of the newest deployment that booted fine with an initramfs of its own that mounts no deployment and opens `archix rescue`, to list and repoint deployments, check the root and the ESP, restore broken files from the package cache, reset a password or eject, without a live USB. Refreshed after each switch, signed when Secure Boot signing is on. It gives whoever is at the console a root shell without a password, as a live USB would, but on an encrypted root it needs the passphrase and it never unlocks a TPM-bound volume; turn it off on machines whose firmware locks out other boot media.",
      },
      health_check = {
        units = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "For servers and headless machines, off by default: systemd units that must be active, such as \"sshd.service\", before a new deployment's boot counts as good. Every target here needs boot.tries > 0. A target that still fails after boot.health_check.timeout marks the deployment's entry bad, so the next boot starts the previous deployment, and the login notes and `archix status` say which target failed and when.",
        },
        listening = archix.option {
          type = t.list_of(t.int),
          default = {},
          description = "TCP ports something on this machine must listen on, such as 22 for sshd, before a new deployment's boot counts as good.",
        },
        reachable = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "Hosts that must answer before a new deployment's boot counts as good: \"host:port\" must accept a TCP connection, a bare host must answer a ping, and \"gateway\" is the default IPv4 gateway. A host that's down for a reason outside the deployment (the router, a captive portal, a laptop on a train) fails the check just the same and falls back from a good update, so list only what is always up when this machine boots; switching again once it answers brings the update back.",
        },
        login = archix.option {
          type = t.bool,
          default = false,
          description = "Each user archix declares must be able to log in: `su -s <shell> <user> -c true` must succeed. Without it, a new deployment's boot still doesn't count as good while a declared user's login shell is missing or not in /etc/shells.",
        },
        timeout = archix.option {
          type = t.int,
          default = 120,
          description = "Seconds the health check keeps retrying its targets after boot before it fails.",
        },
        reboot_on_failure = archix.option {
          type = t.bool,
          default = false,
          description = "Reboot as soon as the health check fails, so the previous deployment starts with nobody at the machine; otherwise the failed deployment keeps running until the next restart.",
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
