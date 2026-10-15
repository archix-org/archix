FROM archlinux:latest
ARG SNAPSHOT=2026/10/10
RUN retry() { for attempt in 1 2 3; do "$@" && return 0; sleep 15; done; return 1; } \
 && echo "Server = https://archive.archlinux.org/repos/$SNAPSHOT/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist \
 && sed -i '/^\[options\]/a DisableDownloadTimeout' /etc/pacman.conf \
 && retry pacman -Syyuu --noconfirm \
 && retry pacman -S --noconfirm --needed rust gcc devtools git base-devel linux mkinitcpio systemd-ukify sbctl btrfs-progs \
 && pacman -Scc --noconfirm
ENV CARGO_TARGET_DIR=/target
WORKDIR /src
