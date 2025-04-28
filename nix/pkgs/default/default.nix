# Dependencies (keep sorted)
{ craneLib
, inputs
, lib
, pkgsBuildHost
, rocksdb
, rust
, stdenv

# Options (keep sorted)
, default-features ? true
, features ? []
, profile ? "release"
}:

let
  buildDepsOnlyEnv =
    let
      rocksdb' = rocksdb.override {
        enableJemalloc = builtins.elem "jemalloc" features;
        enableLiburing = false;
      };
    in
    {
      NIX_OUTPATH_USED_AS_RANDOM_SEED = "randomseed"; # https://crane.dev/faq/rebuilds-bindgen.html
      ROCKSDB_INCLUDE_DIR = "${rocksdb'}/include";
      ROCKSDB_LIB_DIR = "${rocksdb'}/lib";
    }
    //
    (import ./cross-compilation-env.nix {
      # Keep sorted
      inherit
        lib
        pkgsBuildHost
        rust
        stdenv;
    });

  buildPackageEnv = {
    CONDUIT_VERSION_EXTRA = inputs.self.shortRev or inputs.self.dirtyShortRev;
  } // buildDepsOnlyEnv;

  commonAttrs = {
    inherit
      (craneLib.crateNameFromCargoToml {
        cargoToml = "${inputs.self}/Cargo.toml";
      })
      pname
      version;

    src = let filter = inputs.nix-filter.lib; in filter {
      root = inputs.self;

      # Keep sorted
      include = [
        ".cargo"
        "Cargo.lock"
        "Cargo.toml"
        "src"
      ];
    };

    nativeBuildInputs = [
      # bindgen needs the build platform's libclang. Apparently due to "splicing
      # weirdness", pkgs.rustPlatform.bindgenHook on its own doesn't quite do the
      # right thing here.
      pkgsBuildHost.rustPlatform.bindgenHook
    ];

    CARGO_PROFILE = profile;
  };
in

craneLib.buildPackage ( commonAttrs // {
  cargoArtifacts = craneLib.buildDepsOnly (commonAttrs // {
    env = buildDepsOnlyEnv;
  });

  cargoExtraArgs = "--locked "
    + lib.optionalString
      (!default-features)
      "--no-default-features "
    + lib.optionalString
      (features != [])
      "--features " + (builtins.concatStringsSep "," features);

  # This is redundant with CI
  doCheck = false;

  env = buildPackageEnv;

  passthru = {
    env = buildPackageEnv;
  };

  meta.mainProgram = commonAttrs.pname;
})
