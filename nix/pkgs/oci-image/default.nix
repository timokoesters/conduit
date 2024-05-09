# Keep sorted
{
  default,
  dockerTools,
  lib,
  tini,
}:
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
