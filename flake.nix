{
  description = "debug-log-tool: utilities for Bitcoin Core debug.log files";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system:
          f (import nixpkgs { inherit system; }));

      mkPackage = pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "debug-log-tool";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
        };
    in
    {
      devShells = forAllSystems (pkgs:
        let
          # `check` shorthand: runs every `checks.*` derivation in a way that
          # always actually executes them.
          #   - Cold cache  → plain `nix build` (one build, runs the checks).
          #   - Warm cache  → `nix build --rebuild`, which forces a fresh
          #                   re-run and byte-compares against the cached
          #                   output (doubling as a reproducibility check).
          # Forwards extra args, e.g. `check -L`, `check --keep-going`.
          check = pkgs.writeShellApplication {
            name = "check";
            text = ''
              drvs=(
                ".#checks.${pkgs.stdenv.hostPlatform.system}.build"
                ".#checks.${pkgs.stdenv.hostPlatform.system}.clippy"
                ".#checks.${pkgs.stdenv.hostPlatform.system}.fmt"
              )
              if nix path-info "''${drvs[@]}" >/dev/null 2>&1; then
                exec nix build --rebuild --no-link "''${drvs[@]}" "$@"
              else
                exec nix build --no-link "''${drvs[@]}" "$@"
              fi
            '';
          };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              check
            ];

            env.RUST_BACKTRACE = "1";
          };
        });

      packages = forAllSystems (pkgs: {
        default = mkPackage pkgs;
      });

      # `nix flake check` builds and runs every derivation below in parallel,
      # giving a single-command local mirror of the cargo-based CI jobs.
      checks = forAllSystems (pkgs: {
        # cargo build + cargo test (rustPlatform.buildRustPackage runs tests
        # in its default checkPhase).
        build = mkPackage pkgs;

        # cargo clippy --all-targets -- -D warnings, reusing the vendored
        # dependency closure produced by the base package derivation.
        clippy = (mkPackage pkgs).overrideAttrs (old: {
          pname = "${old.pname}-clippy";
          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
          buildPhase = ''
            runHook preBuild
            cargo clippy --all-targets --offline -- -D warnings
            runHook postBuild
          '';
          doCheck = false;
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            runHook postInstall
          '';
        });

        # cargo fmt --all -- --check. No compilation, so no vendoring needed.
        fmt = pkgs.runCommand "debug-log-tool-fmt"
          {
            nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ];
          } ''
            cp -r ${./.} src
            chmod -R u+w src
            cd src
            cargo fmt --all -- --check
            touch $out
          '';
      });
    };
}
