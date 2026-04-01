{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    bold.url = "github:bigsaltyfishes/bold";
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      bold
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.pkgsBuildHost.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        cctools = pkgs.callPackage ./nix/cctools.nix { };
        bold_linux = bold.packages.${system}.default;
      in
      with pkgs;
      {
        devShells.default = mkShell {
          LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
          buildInputs = [
            rustToolchain
            just
            llvm
            lld
            bold_linux
            clang
            cctools
            qemu
            libisoburn
            pkg-config
            openssl.dev
          ];
        };
      }
    );
}
