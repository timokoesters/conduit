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
          [
            "-l"
            "c"

            "-l"
            "stdc++"
            "-L"
            "${stdenv.cc.cc.lib}/${stdenv.hostPlatform.config}/lib"
          ]
      );
}

