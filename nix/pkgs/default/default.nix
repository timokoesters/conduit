# Keep sorted
{ craneLib
, inputs
, lib
, pkgsBuildHost
, rocksdb
, rust
, stdenv
}:

let
  env = {
    CONDUIT_VERSION_EXTRA = inputs.self.shortRev or inputs.self.dirtyShortRev;
    ROCKSDB_INCLUDE_DIR = "${rocksdb}/include";
    ROCKSDB_LIB_DIR = "${rocksdb}/lib";
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
in

craneLib.buildPackage rec {
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
      "Cargo.lock"
      "Cargo.toml"
      "src"
    ];
  };

  # This is redundant with CI
  doCheck = false;

  nativeBuildInputs = [
    # bindgen needs the build platform's libclang. Apparently due to "splicing
    # weirdness", pkgs.rustPlatform.bindgenHook on its own doesn't quite do the
    # right thing here.
    pkgsBuildHost.rustPlatform.bindgenHook
  ];

  inherit env;

  passthru = {
    inherit env;
  };

  meta.mainProgram = pname;
}
