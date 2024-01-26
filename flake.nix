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
        sha256 = "sha256-SXRtAuO4IqNOQq+nLbrsDFbVk+3aVA8NNpSZsKlVH/8=";
      };

      builder = pkgs:
        ((crane.mkLib pkgs).overrideToolchain toolchain).buildPackage;

      nativeBuildInputs = pkgs: [
        # bindgen needs the build platform's libclang. Apparently due to
        # "splicing weirdness", pkgs.rustPlatform.bindgenHook on its own doesn't
        # quite do the right thing here.
        pkgs.buildPackages.rustPlatform.bindgenHook
      ];

      env = pkgs: {
        ROCKSDB_INCLUDE_DIR = "${pkgs.rocksdb}/include";
        ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
      }
      // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isStatic {
        ROCKSDB_STATIC = "";
      }
      // {
        CARGO_BUILD_RUSTFLAGS = let inherit (pkgs) lib stdenv; in
          lib.concatStringsSep " " ([]
            ++ lib.optionals
              # This disables PIE for static builds, which isn't great in terms
              # of security. Unfortunately, my hand is forced because nixpkgs'
              # `libstdc++.a` is built without `-fPIE`, which precludes us from
              # leaving PIE enabled.
              stdenv.hostPlatform.isStatic
              ["-C" "relocation-model=static"]
            ++ lib.optionals
              (stdenv.buildPlatform.config != stdenv.hostPlatform.config)
              ["-l" "c"]
            ++ lib.optionals
              # This check has to match the one [here][0]. We only need to set
              # these flags when using a different linker. Don't ask me why,
              # though, because I don't know. All I know is it breaks otherwise.
              #
              # [0]: https://github.com/NixOS/nixpkgs/blob/612f97239e2cc474c13c9dafa0df378058c5ad8d/pkgs/build-support/rust/lib/default.nix#L36-L39
              (
                # Nixpkgs doesn't check for x86_64 here but we do, because I
                # observed a failure building statically for x86_64 without
                # including it here. Linkers are weird.
                (stdenv.hostPlatform.isAarch64 || stdenv.hostPlatform.isx86_64)
                  && stdenv.hostPlatform.isStatic
                  && !stdenv.isDarwin
                  && !stdenv.cc.bintools.isLLVM
              )
              [
                "-l"
                "stdc++"
                "-L"
                "${stdenv.cc.cc.lib}/${stdenv.hostPlatform.config}/lib"
              ]
          );
      }

      # What follows is stolen from [here][0]. Its purpose is to properly
      # configure compilers and linkers for various stages of the build, and
      # even covers the case of build scripts that need native code compiled and
      # run on the build platform (I think).
      #
      # [0]: https://github.com/NixOS/nixpkgs/blob/612f97239e2cc474c13c9dafa0df378058c5ad8d/pkgs/build-support/rust/lib/default.nix#L64-L78
      // (
        let
          inherit (pkgs.rust.lib) envVars;
        in
        pkgs.lib.optionalAttrs
          (pkgs.stdenv.targetPlatform.rust.rustcTarget
            != pkgs.stdenv.hostPlatform.rust.rustcTarget)
          (
            let
              inherit (pkgs.stdenv.targetPlatform.rust) cargoEnvVarTarget;
            in
            {
              "CC_${cargoEnvVarTarget}" = envVars.ccForTarget;
              "CXX_${cargoEnvVarTarget}" = envVars.cxxForTarget;
              "CARGO_TARGET_${cargoEnvVarTarget}_LINKER" =
                envVars.linkerForTarget;
            }
          )
      // (
        let
          inherit (pkgs.stdenv.hostPlatform.rust) cargoEnvVarTarget rustcTarget;
        in
        {
          "CC_${cargoEnvVarTarget}" = envVars.ccForHost;
          "CXX_${cargoEnvVarTarget}" = envVars.cxxForHost;
          "CARGO_TARGET_${cargoEnvVarTarget}_LINKER" = envVars.linkerForHost;
          CARGO_BUILD_TARGET = rustcTarget;
        }
      )
      // (
        let
          inherit (pkgs.stdenv.buildPlatform.rust) cargoEnvVarTarget;
        in
        {
          "CC_${cargoEnvVarTarget}" = envVars.ccForBuild;
          "CXX_${cargoEnvVarTarget}" = envVars.cxxForBuild;
          "CARGO_TARGET_${cargoEnvVarTarget}_LINKER" = envVars.linkerForBuild;
          HOST_CC = "${pkgs.buildPackages.stdenv.cc}/bin/cc";
          HOST_CXX = "${pkgs.buildPackages.stdenv.cc}/bin/c++";
        }
      ));

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

      mkOciImage = pkgs: package:
        pkgs.dockerTools.buildImage {
          name = package.pname;
          tag = "latest";
          config = {
            # Use the `tini` init system so that signals (e.g. ctrl+c/SIGINT)
            # are handled as expected
            Entrypoint = [
              "${pkgs.lib.getExe' pkgs.tini "tini"}"
              "--"
            ];
            Cmd = [
              "${pkgs.lib.getExe package}"
            ];
          };
        };
    in
    {
      packages = {
        default = package pkgsHost;

        oci-image = mkOciImage pkgsHost self.packages.${system}.default;

        # Build an OCI image from the musl aarch64 build so we don't have to
        # build for aarch64 twice (to make a gnu version specifically for the
        # OCI image)
        oci-image-aarch64-unknown-linux-musl = mkOciImage
          pkgsHost
          self.packages.${system}.static-aarch64-unknown-linux-musl;

        # Don't build a musl x86_64 OCI image because that would be pointless.
        # Just use the gnu one (i.e. `self.packages."x86_64-linux".oci-image`).
      } // builtins.listToAttrs (
        builtins.map
          (crossSystem: {
            name = "static-${crossSystem}";
            value = package (import nixpkgs {
              inherit system;
              crossSystem = {
                config = crossSystem;
              };
            }).pkgsStatic;
          })
          [
            "x86_64-unknown-linux-musl"
            "aarch64-unknown-linux-musl"
          ]
      );

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
