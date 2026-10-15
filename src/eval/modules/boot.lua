local t = archix.types

return {
  options = {
    boot = {
      loader = archix.option {
        type = t.nullable(t.enum { "systemd-boot" }),
        default = archix.null,
        description = "The boot loader archix writes a boot entry per generation for; null leaves booting alone.",
      },
      esp = archix.option { type = t.path, default = "/boot", description = "Where the EFI system partition is mounted." },
      kernel = archix.option { type = t.str, default = "linux", description = "The kernel package boot entries use." },
      microcode = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "Microcode images on the ESP loaded before the initramfs, such as amd-ucode.img.",
      },
      entries = archix.option {
        type = t.int,
        default = 5,
        description = "How many of the newest generations keep a boot entry; fewer if the ESP runs out of room.",
      },
      secure_boot = archix.option {
        type = t.nullable(t.bool),
        default = archix.null,
        description = "Sign boot entries with sbctl; by default, whenever sbctl has keys.",
      },
      snapshots = archix.option {
        type = t.bool,
        default = true,
        description = "On a btrfs root, take a read-only snapshot of / before every switch.",
      },
    },
  },
}
