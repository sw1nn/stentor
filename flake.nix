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

            # whisper-rs-sys builds whisper.cpp via cmake, with the vulkan
            # feature compiling ggml-vulkan's shaders through glslc (shaderc).
            # openmp needs no extra package: it links against libgomp, which
            # ships with the gcc from nixpkgs' default stdenv used to compile
            # whisper.cpp's C/C++ sources.
            cmake
            shaderc
            vulkan-headers

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
            fontconfig
            libpulseaudio

            # GUI (gtk4-sys, libadwaita-sys, and friends)
            gtk4
            libadwaita

            # whisper-rs's vulkan feature links libvulkan directly
            # (cargo:rustc-link-lib=vulkan in whisper-rs-sys and gdk4-sys)
            vulkan-loader
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
