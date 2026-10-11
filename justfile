# Run `nix develop` (or use direnv) first so every tool is on PATH.

set shell := ["bash", "-euo", "pipefail", "-c"]

# List the recipes.
default:
    @just --list

# Build the binary.
build:
    cargo build

# Format every source.
fmt:
    cargo fmt --all
    nix fmt flake.nix

# Check formatting without changing files.
fmt-check:
    cargo fmt --all --check

# Lint with every warning as an error.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the tests.
test:
    cargo test

# Everything CI runs.
ci: fmt-check lint test

# What the release workflow will publish, minus the publishing. Worth running
# before publishing a release: crates.io never lets a version be replaced.
release-check:
    cargo publish --dry-run --locked
