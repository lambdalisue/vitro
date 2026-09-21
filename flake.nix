{
  description = "vitro — disposable development VMs driven from a CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Single source of the Rust version, shared by the dev shell and (once
        # there is a crate to build) the package build.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        # `nix develop` — the pinned tools, same ones CI uses via `just`.
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain

            # vitro drives guests by spawning these; pinning them keeps the
            # QEMU command line reproducible across contributors.
            qemu
            openssh

            just
            git
            jq
            nixpkgs-fmt
          ];

          env = {
            RUST_BACKTRACE = "1";

            # Where QEMU's bundled firmware blobs live. A dev-shell convenience
            # for the experiment scripts; the CLI itself resolves firmware from
            # its own config, never from the environment.
            VITRO_DEV_QEMU_SHARE = "${pkgs.qemu}/share/qemu";
          };

          shellHook = ''
            echo "vitro dev shell — $(rustc --version), $(qemu-system-aarch64 --version | head -1)"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
