FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed rust gcc && pacman -Scc --noconfirm
ENV CARGO_TARGET_DIR=/target
WORKDIR /src
