# Keep sorted
{ default
, dockerTools
, lib
, pkgs
}:
let
  # See https://github.com/krallin/tini/pull/223
  tini = pkgs.tini.overrideAttrs {
    patches = [ (pkgs.fetchpatch {
        url = "https://patch-diff.githubusercontent.com/raw/krallin/tini/pull/223.patch";
        hash = "sha256-i6xcf+qpjD+7ZQY3ueiDaxO4+UA2LutLCZLNmT+ji1s=";
      })
    ];
  };
in
dockerTools.buildImage {
  name = default.pname;
  tag = "next";
  copyToRoot = [
    dockerTools.caCertificates
  ];
  config = {
    # Use the `tini` init system so that signals (e.g. ctrl+c/SIGINT)
    # are handled as expected
    Entrypoint = [
      "${lib.getExe' tini "tini"}"
      "--"
    ];
    Cmd = [
      "${lib.getExe default}"
    ];
  };
}
