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

# What CI will do with a tag, minus the uploading. Worth running before tagging:
# the release workflow's own dry run costs six runners and several minutes.
release-check:
    cargo publish --dry-run --locked
