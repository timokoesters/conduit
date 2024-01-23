{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    nix-filter.url = "github:numtide/nix-filter";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    attic.url = "github:zhaofengli/attic?ref=main";
  };

  outputs =
    { self
    , nixpkgs
    , flake-utils
    , nix-filter

    , fenix
    , crane
    , ...
    }: flake-utils.lib.eachDefaultSystem (system:
    let
      pkgsHost = nixpkgs.legacyPackages.${system};

      # Nix-accessible `Cargo.toml`
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

      # The Rust toolchain to use
      toolchain = fenix.packages.${system}.toolchainOf {
        # Use the Rust version defined in `Cargo.toml`
        channel = cargoToml.package.rust-version;

        # THE rust-version HASH
        sha256 = "sha256-gdYqng0y9iHYzYPAdkC/ka3DRny3La/S5G8ASj0Ayyc=";
      };

      mkToolchain = fenix.packages.${system}.combine;

      buildToolchain = mkToolchain (with toolchain; [
        cargo
        rustc
      ]);

      devToolchain = mkToolchain (with toolchain; [
        cargo
        clippy
        rust-src
        rustc

        # Always use nightly rustfmt because most of its options are unstable
        fenix.packages.${system}.latest.rustfmt
      ]);

      builder = pkgs:
        ((crane.mkLib pkgs).overrideToolchain buildToolchain).buildPackage;

      nativeBuildInputs = pkgs: [
        pkgs.rustPlatform.bindgenHook
      ];

      env = pkgs: {
        ROCKSDB_INCLUDE_DIR = "${pkgs.rocksdb}/include";
        ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
      };
    in
    {
      packages.default = builder pkgsHost {
        src = nix-filter {
          root = ./.;
          include = [
            "src"
            "Cargo.toml"
            "Cargo.lock"
          ];
        };

        # This is redundant with CI
        doCheck = false;

        env = env pkgsHost;
        nativeBuildInputs = nativeBuildInputs pkgsHost;

        meta.mainProgram = cargoToml.package.name;
      };

      packages.oci-image =
      let
        package = self.packages.${system}.default;
      in
      pkgsHost.dockerTools.buildImage {
        name = package.pname;
        tag = "latest";
        config = {
          # Use the `tini` init system so that signals (e.g. ctrl+c/SIGINT) are
          # handled as expected
          Entrypoint = [
            "${pkgsHost.lib.getExe' pkgsHost.tini "tini"}"
            "--"
          ];
          Cmd = [
            "${pkgsHost.lib.getExe package}"
          ];
        };
      };

      devShells.default = pkgsHost.mkShell {
        env = env pkgsHost // {
          # Rust Analyzer needs to be able to find the path to default crate
          # sources, and it can read this environment variable to do so. The
          # `rust-src` component is required in order for this to work.
          RUST_SRC_PATH = "${devToolchain}/lib/rustlib/src/rust/library";
        };

        # Development tools
        nativeBuildInputs = nativeBuildInputs pkgsHost ++ [
          devToolchain
        ] ++ (with pkgsHost; [
          engage
        ]);
      };
    });
}
