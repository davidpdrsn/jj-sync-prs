{
  description = "A Rust project built with Nix";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
        };
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "jj-sync-prs";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = [
            pkgs.makeWrapper
          ];

          buildInputs = [
            pkgs.graphviz
          ];

          postInstall = ''
            wrapProgram $out/bin/jj-sync-prs \
              --prefix PATH : ${pkgs.lib.makeBinPath [pkgs.graphviz]}
          '';
        };
      }
    );
}
