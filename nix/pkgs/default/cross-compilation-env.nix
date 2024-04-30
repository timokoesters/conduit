{ lib
, pkgsBuildHost
, rust
, stdenv
}:

lib.optionalAttrs stdenv.hostPlatform.isStatic {
  ROCKSDB_STATIC = "";
}
//
{
  CARGO_BUILD_RUSTFLAGS =
    lib.concatStringsSep
      " "
      ([]
        # This disables PIE for static builds, which isn't great in terms of
        # security. Unfortunately, my hand is forced because nixpkgs'
        # `libstdc++.a` is built without `-fPIE`, which precludes us from
        # leaving PIE enabled.
        ++ lib.optionals
          stdenv.hostPlatform.isStatic
          [ "-C" "relocation-model=static" ]
        ++ lib.optionals
          (stdenv.buildPlatform.config != stdenv.hostPlatform.config)
          [ "-l" "c" ]
        ++ lib.optionals
          # This check has to match the one [here][0]. We only need to set
          # these flags when using a different linker. Don't ask me why, though,
          # because I don't know. All I know is it breaks otherwise.
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

# What follows is stolen from [here][0]. Its purpose is to properly configure
# compilers and linkers for various stages of the build, and even covers the
# case of build scripts that need native code compiled and run on the build
# platform (I think).
#
# [0]: https://github.com/NixOS/nixpkgs/blob/5cdb38bb16c6d0a38779db14fcc766bc1b2394d6/pkgs/build-support/rust/lib/default.nix#L57-L80
//
(
  let
    inherit (rust.lib) envVars;
  in
  lib.optionalAttrs
    (stdenv.targetPlatform.rust.rustcTarget
      != stdenv.hostPlatform.rust.rustcTarget)
    (
      let
        inherit (stdenv.targetPlatform.rust) cargoEnvVarTarget;
      in
      {
        "CC_${cargoEnvVarTarget}" = envVars.ccForTarget;
        "CXX_${cargoEnvVarTarget}" = envVars.cxxForTarget;
        "CARGO_TARGET_${cargoEnvVarTarget}_LINKER" =
          envVars.linkerForTarget;
      }
    )
  //
  (
    let
      inherit (stdenv.hostPlatform.rust) cargoEnvVarTarget rustcTarget;
    in
    {
      "CC_${cargoEnvVarTarget}" = envVars.ccForHost;
      "CXX_${cargoEnvVarTarget}" = envVars.cxxForHost;
      "CARGO_TARGET_${cargoEnvVarTarget}_LINKER" = envVars.linkerForHost;
      CARGO_BUILD_TARGET = rustcTarget;
    }
  )
  //
  (
    let
      inherit (stdenv.buildPlatform.rust) cargoEnvVarTarget;
    in
    {
      "CC_${cargoEnvVarTarget}" = envVars.ccForBuild;
      "CXX_${cargoEnvVarTarget}" = envVars.cxxForBuild;
      "CARGO_TARGET_${cargoEnvVarTarget}_LINKER" = envVars.linkerForBuild;
      HOST_CC = "${pkgsBuildHost.stdenv.cc}/bin/cc";
      HOST_CXX = "${pkgsBuildHost.stdenv.cc}/bin/c++";
    }
  )
)
