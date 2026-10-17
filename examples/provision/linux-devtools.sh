#!/bin/sh
# Provision a Debian or Ubuntu golden image for building native code: a C
# toolchain, pkg-config, Git, curl, and Rust through rustup.
#
#   [images.linux.build]
#   kind      = "cloud-image"
#   source    = "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-arm64.qcow2"
#   provision = "~/path/to/linux-devtools.sh"
#
# It runs as the guest account, which has passwordless sudo, and can be run
# again: whatever is already there is left alone.
set -eu

sudo DEBIAN_FRONTEND=noninteractive apt-get update -q
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q \
  build-essential pkg-config git curl ca-certificates

if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain stable
fi

# rustup appends its line to ~/.bashrc, but Debian's ~/.bashrc returns early
# for a shell that is not interactive, which is what `vitro exec` gets — so
# cargo would only be found in `vitro ssh`. The line goes at the top instead.
line='. "$HOME/.cargo/env"'
if ! head -n 1 "$HOME/.bashrc" 2>/dev/null | grep -qxF "$line"; then
  { printf '%s\n' "$line"; cat "$HOME/.bashrc" 2>/dev/null || true; } > "$HOME/.bashrc.vitro"
  mv "$HOME/.bashrc.vitro" "$HOME/.bashrc"
fi

. "$HOME/.cargo/env"
cc --version | head -n 1
git --version
cargo --version
