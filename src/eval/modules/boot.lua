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
      revoke_unused = archix.option {
        type = t.bool,
        default = false,
        description = "Experimental, tested on OVMF only: after every switch, add the boot images archix signed and no entry uses any more to dbx, as `archix secure-boot revoke` does, so the firmware refuses them. Needs Secure Boot keys from sbctl as files, and declares efitools.",
      },
    },
  },
}
