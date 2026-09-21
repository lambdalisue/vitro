default: check

# Everything CI runs, in the order that fails fastest.
check: fmt-check lint test

build:
    cargo build

test:
    cargo test

fmt:
    cargo fmt
    nixpkgs-fmt flake.nix

fmt-check:
    cargo fmt --check
    nixpkgs-fmt --check flake.nix

lint:
    cargo clippy --all-targets --all-features -- -D warnings

# The spikes are separate crates on purpose; build them only when a design
# question is being revisited.
spikes:
    cargo build --release --manifest-path spikes/seedfat/Cargo.toml
    cargo build --release --manifest-path spikes/portprobe/Cargo.toml
    cargo build --release --manifest-path spikes/hostfacts/Cargo.toml
