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
      # TODO: Switch back to upstream after [this issue][0] is fixed
      #
      # [0]: https://github.com/ipetkov/crane/issues/497
      url = "github:CobaltCause/crane?ref=crimes-for-cross";
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
      toolchain = fenix.packages.${system}.fromToolchainFile {
        file = ./rust-toolchain.toml;

        # See also `rust-toolchain.toml`
        sha256 = "sha256-gdYqng0y9iHYzYPAdkC/ka3DRny3La/S5G8ASj0Ayyc=";
      };

      builder = pkgs:
        ((crane.mkLib pkgs).overrideToolchain toolchain).buildPackage;

      nativeBuildInputs = pkgs: [
        pkgs.rustPlatform.bindgenHook
      ];

      env = pkgs: {
        ROCKSDB_INCLUDE_DIR = "${pkgs.rocksdb}/include";
        ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
      };

      package = pkgs: builder pkgs {
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

        env = env pkgs;
        nativeBuildInputs = nativeBuildInputs pkgs;

        meta.mainProgram = cargoToml.package.name;
      };
    in
    {
      packages = {
        default = package pkgsHost;

        oci-image =
        let
          package = self.packages.${system}.default;
        in
        pkgsHost.dockerTools.buildImage {
          name = package.pname;
          tag = "latest";
          config = {
            # Use the `tini` init system so that signals (e.g. ctrl+c/SIGINT)
            # are handled as expected
            Entrypoint = [
              "${pkgsHost.lib.getExe' pkgsHost.tini "tini"}"
              "--"
            ];
            Cmd = [
              "${pkgsHost.lib.getExe package}"
            ];
          };
        };
      };

      devShells.default = pkgsHost.mkShell {
        env = env pkgsHost // {
          # Rust Analyzer needs to be able to find the path to default crate
          # sources, and it can read this environment variable to do so. The
          # `rust-src` component is required in order for this to work.
          RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
        };

        # Development tools
        nativeBuildInputs = nativeBuildInputs pkgsHost ++ [
          # Always use nightly rustfmt because most of its options are unstable
          #
          # This needs to come before `toolchain` in this list, otherwise
          # `$PATH` will have stable rustfmt instead.
          fenix.packages.${system}.latest.rustfmt

          toolchain
        ] ++ (with pkgsHost; [
          engage
        ]);
      };
    });
}
