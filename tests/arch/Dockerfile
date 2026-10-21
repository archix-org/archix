FROM archlinux:latest
ARG SNAPSHOT=2026/10/10
# An optional build secret "archive" names a caching mirror for the install; a secret isn't part of the layer cache key.
RUN --mount=type=secret,id=archive retry() { for attempt in 1 2 3; do "$@" && return 0; sleep 15; done; return 1; } \
 && archive=$(cat /run/secrets/archive 2>/dev/null || echo https://archive.archlinux.org) \
 && echo "Server = $archive/repos/$SNAPSHOT/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist \
 && sed -i '/^\[options\]/a DisableDownloadTimeout' /etc/pacman.conf \
 && retry pacman -Syyuu --noconfirm \
 && retry pacman -S --noconfirm --needed rust gcc devtools git base-devel linux mkinitcpio systemd-ukify sbctl btrfs-progs \
    qemu-base edk2-ovmf arch-install-scripts dosfstools rust-musl musl \
 && echo "Server = https://archive.archlinux.org/repos/$SNAPSHOT/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist \
 && pacman -Scc --noconfirm
# The VM suites' TPM (pcr) and ssh client (firewall), in a layer of their own so the one above stays cached.
RUN pacman -S --noconfirm --needed swtpm openssh && pacman -Scc --noconfirm
ENV CARGO_TARGET_DIR=/target
WORKDIR /src
