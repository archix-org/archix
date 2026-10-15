FROM archlinux:latest
ARG SNAPSHOT=2026/10/10
RUN echo "Server = https://archive.archlinux.org/repos/$SNAPSHOT/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist \
 && pacman -Syyuu --noconfirm \
 && pacman -S --noconfirm --needed rust gcc devtools git \
 && pacman -Scc --noconfirm
ENV CARGO_TARGET_DIR=/target
WORKDIR /src
