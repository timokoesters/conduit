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
    # Pinned because crane's own automatic cross compilation configuration that they
    # introduce in the next commit attempts to link the musl targets against glibc
    # for some reason. Unpin once this is fixed.
    crane.url = "github:ipetkov/crane?rev=bb1c9567c43e4434f54e9481eb4b8e8e0d50f0b5";
    attic.url = "github:zhaofengli/attic?ref=main";
  };

  outputs = inputs:
    let
      # Keep sorted
      mkScope = pkgs: pkgs.lib.makeScope pkgs.newScope (self: {
        craneLib =
          (inputs.crane.mkLib pkgs).overrideToolchain (_: self.toolchain);

        default = self.callPackage ./nix/pkgs/default {};

        inherit inputs;

        oci-image = self.callPackage ./nix/pkgs/oci-image {};

        book = self.callPackage ./nix/pkgs/book {};

        rocksdb =
        let
          version = "10.2.1";
        in
        pkgs.rocksdb.overrideAttrs (old: {
          inherit version;
          src = pkgs.fetchFromGitHub {
            owner = "facebook";
            repo = "rocksdb";
            rev = "v${version}";
            hash = "sha256-v8kZShgz0O3nHZwWjTvhcM56qAs/le1XgMVYyvVd4tg=";
          };
        });

        shell = self.callPackage ./nix/shell.nix {};

        # The Rust toolchain to use
        toolchain = inputs
          .fenix
          .packages
          .${pkgs.pkgsBuildHost.system}
          .fromToolchainFile {
            file = ./rust-toolchain.toml;

            # See also `rust-toolchain.toml`
            sha256 = "sha256-AJ6LX/Q/Er9kS15bn9iflkUwcgYqRQxiOIL2ToVAXaU=";
          };
      });
    in
    inputs.flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = (import inputs.nixpkgs {
          inherit system;
          
          # libolm is deprecated, but we only need it for complement
          config.permittedInsecurePackages = [
            "olm-3.2.16"
          ];
        });
      in
      {
        packages = {
          default = (mkScope pkgs).default;
          oci-image = (mkScope pkgs).oci-image;
          book = (mkScope pkgs).book;
        }
        //
        builtins.listToAttrs
          (builtins.concatLists
            (builtins.map
              (crossSystem:
                let
                  binaryName = "static-${crossSystem}";
                  pkgsCrossStatic =
                    (import inputs.nixpkgs {
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
                    value = (mkScope pkgsCrossStatic).default;
                  }

                  # An output for an OCI image based on that binary
                  {
                    name = "oci-image-${crossSystem}";
                    value = (mkScope pkgsCrossStatic).oci-image;
                  }
                ]
              )
              [
                "x86_64-unknown-linux-musl"
                "aarch64-unknown-linux-musl"
              ]
            )
          );

        devShells.default = (mkScope pkgs).shell;
      }
    );
}
