{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    nix-filter.url = "github:numtide/nix-filter";
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane?ref=master";
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
        pkgs.pkgsBuildHost.rustPlatform.bindgenHook
      ];

      rocksdb' = pkgs:
      let
        version = "9.1.0";
      in
      pkgs.rocksdb.overrideAttrs (old: {
        inherit version;
        src = pkgs.fetchFromGitHub {
          owner = "facebook";
          repo = "rocksdb";
          rev = "v${version}";
          hash = "sha256-vRPyrXkXVVhP56n5FVYef8zbIsnnanQSpElmQLZ7mh8=";
        };
      });

      env = pkgs: {
        CONDUIT_VERSION_EXTRA = self.shortRev or self.dirtyShortRev;
        ROCKSDB_INCLUDE_DIR = "${rocksdb' pkgs}/include";
        ROCKSDB_LIB_DIR = "${rocksdb' pkgs}/lib";
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
              # [0]: https://github.com/NixOS/nixpkgs/blob/5cdb38bb16c6d0a38779db14fcc766bc1b2394d6/pkgs/build-support/rust/lib/default.nix#L37-L40
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
      # [0]: https://github.com/NixOS/nixpkgs/blob/5cdb38bb16c6d0a38779db14fcc766bc1b2394d6/pkgs/build-support/rust/lib/default.nix#L57-L80
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
          HOST_CC = "${pkgs.pkgsBuildHost.stdenv.cc}/bin/cc";
          HOST_CXX = "${pkgs.pkgsBuildHost.stdenv.cc}/bin/c++";
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
          tag = "next";
          copyToRoot = [
            pkgs.dockerTools.caCertificates
          ];
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

        book =
        let
          package = self.packages.${system}.default;
        in
        pkgsHost.stdenv.mkDerivation {
          pname = "${package.pname}-book";
          version = package.version;

          src = nix-filter {
            root = ./.;
            include = [
              "book.toml"
              "conduit-example.toml"
              "README.md"
              "debian/README.md"
              "docs"
            ];
          };

          nativeBuildInputs = (with pkgsHost; [
            mdbook
          ]);

          buildPhase = ''
            mdbook build
            mv public $out
          '';
        };
      }
      //
      builtins.listToAttrs
        (builtins.concatLists
          (builtins.map
            (crossSystem:
              let
                binaryName = "static-${crossSystem}";
                pkgsCrossStatic =
                  (import nixpkgs {
                    inherit system;
                    crossSystem = {
                      config = crossSystem;
                    };
                  }).pkgsStatic;
              in
              [
                # An output for a statically-linked binary
                {
                  name = binaryName;
                  value = package pkgsCrossStatic;
                }

                # An output for an OCI image based on that binary
                {
                  name = "oci-image-${crossSystem}";
                  value = mkOciImage
                    pkgsCrossStatic
                    self.packages.${system}.${binaryName};
                }
              ]
            )
            [
              "x86_64-unknown-linux-musl"
              "aarch64-unknown-linux-musl"
            ]
          )
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

          # Needed for producing Debian packages
          cargo-deb

          # Needed for Complement
          go
          olm

          # Needed for our script for Complement
          jq

          # Needed for finding broken markdown links
          lychee

          # Useful for editing the book locally
          mdbook
        ]);
      };
    });
}
