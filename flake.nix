{
  description = "verandah development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustVersion = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          name = "stentor dev";
          nativeBuildInputs = with pkgs; [
            # Build tools and compilers
            pkg-config
            clang

            # Rust toolchain from rust-toolchain.toml
            (rustVersion.override { extensions = [ "rust-src" "llvm-tools-preview" ]; })
            rust-analyzer

            # Development tools
            cargo-nextest
            cargo-udeps
            cargo-llvm-cov
            bacon
            taplo
            rust-code-analysis
          ];

          buildInputs = with pkgs; [
            # Runtime libraries
            openssl
            hidapi
            libusb1
            imagemagick
            fontconfig
            libpulseaudio
            sqlite

            # Hyprland libraries
            hyprland
          ];

          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

          # Set plugin path to cargo build output for development
          # Uses CARGO_TARGET_DIR if set, otherwise defaults to ./target
          shellHook = ''
            export VERANDAH_PLUGIN_PATH="''${CARGO_TARGET_DIR:-$PWD/target}/debug"
          '';
        };
      }
    );
}
