local t = archix.types

return {
  options = {
    rollback = {
      enable = archix.option {
        type = t.bool,
        default = false,
        description = "Take a read-only btrfs snapshot of the root before each switch and each plain pacman transaction, to roll back to or boot. Needs the root on a btrfs subvolume and `archix snapshot setup`; on ext4 there are no rollbacks, and everything else works as usual.",
      },
      keep_switch = archix.option {
        type = t.int,
        default = 10,
        description = "How many snapshots taken by switches to keep, newest first; ones with a boot entry are never dropped.",
      },
      keep_pacman = archix.option {
        type = t.int,
        default = 10,
        description = "How many snapshots taken before plain pacman transactions to keep, newest first; ones with a boot entry are never dropped.",
      },
      min_free = archix.option {
        type = t.int,
        default = 10,
        description = "Below this percentage of free space on the root, archix warns and drops the oldest snapshots it may, never manual ones, ones with a boot entry or the newest.",
      },
      entries = archix.option {
        type = t.int,
        default = 5,
        description = "How many of the newest snapshots get a boot entry (snapshots taken by switches, and by pacman transactions that changed the kernel or initramfs), besides the fallback slot: the first snapshot since the last boot that counted as good.",
      },
    },
  },
}
