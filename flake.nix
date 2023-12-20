{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs =
    { self
    , nixpkgs
    , flake-utils

    , fenix
    , crane
    }: flake-utils.lib.eachDefaultSystem (system:
    let
      pkgs = nixpkgs.legacyPackages.${system};

      # Use mold on Linux
      stdenv = if pkgs.stdenv.isLinux then
        pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv
      else
        pkgs.stdenv;

      # Nix-accessible `Cargo.toml`
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

      # The Rust toolchain to use
      toolchain = fenix.packages.${system}.toolchainOf {
        # Use the Rust version defined in `Cargo.toml`
        channel = cargoToml.package.rust-version;

        # THE rust-version HASH
        sha256 = "sha256-gdYqng0y9iHYzYPAdkC/ka3DRny3La/S5G8ASj0Ayyc=";
      };

      # The system's RocksDB
      ROCKSDB_INCLUDE_DIR = "${pkgs.rocksdb}/include";
      ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";

      # Shared between the package and the devShell
      nativeBuildInputs = (with pkgs.rustPlatform; [
        bindgenHook
      ]);

      builder =
        ((crane.mkLib pkgs).overrideToolchain toolchain.toolchain).buildPackage;
    in
    {
      packages.default = builder {
        src = ./.;

        inherit
          stdenv
          nativeBuildInputs
          ROCKSDB_INCLUDE_DIR
          ROCKSDB_LIB_DIR;
      };

      devShells.default = (pkgs.mkShell.override { inherit stdenv; }) {
        # Rust Analyzer needs to be able to find the path to default crate
        # sources, and it can read this environment variable to do so
        RUST_SRC_PATH = "${toolchain.rust-src}/lib/rustlib/src/rust/library";

        inherit
          ROCKSDB_INCLUDE_DIR
          ROCKSDB_LIB_DIR;

        # Development tools
        nativeBuildInputs = nativeBuildInputs ++ (with toolchain; [
          cargo
          clippy
          rust-src
          rustc
          rustfmt
        ]) ++ (with pkgs; [
          engage
        ]);
      };

      checks = {
        packagesDefault = self.packages.${system}.default;
        devShellsDefault = self.devShells.${system}.default;
      };
    });
}
