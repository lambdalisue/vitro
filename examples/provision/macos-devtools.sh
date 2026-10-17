#!/bin/bash
# Provision a macOS golden image (built through lume's unattended preset) for
# building native code: the Command Line Tools (clang, git, make), Rust through
# rustup, and Homebrew.
#
#   [images.macos.build]
#   kind       = "lume-ipsw"
#   source     = "latest"
#   unattended = "tahoe"
#   provision  = "~/path/to/macos-devtools.sh"
#
# The preset's account needs its password for sudo, and lume's presets set it
# to the account name, so `lume` unless VITRO_SUDO_PASSWORD says otherwise.
# Whatever is already installed is left alone, so the script can also bring a
# running VM up to date with `vitro exec --script`.
set -euo pipefail

password="${VITRO_SUDO_PASSWORD:-lume}"
askpass="$(mktemp)"
trap 'rm -f "$askpass"' EXIT
printf '#!/bin/sh\nprintf "%%s\\n" %q\n' "$password" > "$askpass"
chmod 700 "$askpass"
export SUDO_ASKPASS="$askpass"
sudo -A -v

# The Command Line Tools. Their usual installer is a dialog nobody can answer
# over SSH; this file makes softwareupdate offer them instead.
if ! xcode-select -p > /dev/null 2>&1; then
  marker=/tmp/.com.apple.dt.CommandLineTools.installondemand.in-progress
  touch "$marker"
  label=$(softwareupdate -l 2>/dev/null |
    sed -n 's/^\* Label: \(Command Line Tools.*\)$/\1/p' | tail -n 1)
  if [ -z "$label" ]; then
    echo "softwareupdate offers no Command Line Tools" >&2
    exit 1
  fi
  sudo -A softwareupdate -i "$label" --verbose
  rm -f "$marker"
fi

if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain stable
fi

# Homebrew asks for the password through sudo, which it finds through
# SUDO_ASKPASS when it may not prompt.
if [ ! -x /opt/homebrew/bin/brew ]; then
  NONINTERACTIVE=1 /bin/bash -c \
    "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi
# ~/.zshenv, not ~/.zprofile as Homebrew suggests: `vitro exec` gets a shell
# that is not a login shell, and only ~/.zshenv is read by every zsh. rustup
# puts its own line there for the same reason.
if ! grep -qs 'brew shellenv' "$HOME/.zshenv"; then
  echo 'eval "$(/opt/homebrew/bin/brew shellenv)"' >> "$HOME/.zshenv"
fi

. "$HOME/.cargo/env"
clang --version | head -n 1
git --version
cargo --version
/opt/homebrew/bin/brew --version | head -n 1
