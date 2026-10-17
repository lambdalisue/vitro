{
  description = "Disposable development VMs driven from a CLI";

  # CI pushes main's builds here. Nix asks before trusting a flake's
  # substituter, so this offers the cache rather than imposing it.
  nixConfig = {
    extra-substituters = [ "https://vitro.cachix.org" ];
    extra-trusted-public-keys = [ "vitro.cachix.org-1:649aPpp2DJa0LqG49qsSUpoE50NavN5WGKwaLwyL+h8=" ];
  };

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
    rust-overlay = {
      url = "git+https://github.com/oxalica/rust-overlay?shallow=1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          inherit (pkgs) lib;
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain (_: rust);
          package = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
          build = {
            pname = "vitro";
            # The source carries no tags, so a build names the commit it is made from.
            version = "${package.version}-${self.shortRev or self.dirtyShortRev or "unknown"}";
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
              ];
            };
            strictDeps = true;
            buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
            # The tests run in CI and with `just test`.
            doCheck = false;
          };
          vitro = craneLib.buildPackage (
            build
            // {
              cargoArtifacts = craneLib.buildDepsOnly build;
              nativeBuildInputs = [ pkgs.installShellFiles ];
              # The scripts come from the binary itself, so they can only be
              # made where it runs.
              postInstall = lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
                installShellCompletion --cmd vitro \
                  --bash <($out/bin/vitro completions bash) \
                  --fish <($out/bin/vitro completions fish) \
                  --zsh <($out/bin/vitro completions zsh)
              '';
              meta = {
                description = "Disposable development VMs driven from a CLI";
                homepage = "https://github.com/lambdalisue/vitro";
                license = lib.licenses.mit;
                mainProgram = "vitro";
              };
            }
          );
        in
        {
          inherit vitro;
          default = vitro;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in
        {
          default = pkgs.mkShell {
            packages = [
              rust
              pkgs.just
              # vitro drives guests by spawning these; pinning them keeps the
              # QEMU command line reproducible across contributors.
              pkgs.qemu
              pkgs.openssh
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
