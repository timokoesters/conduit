# Keep sorted
{ cargo-deb
, default
, engage
, go
, inputs
, jq
, lychee
, mdbook
, mkShell
, olm
, system
, taplo
, toolchain
, typos
}:

mkShell {
  env = default.env // {
    # Rust Analyzer needs to be able to find the path to default crate
    # sources, and it can read this environment variable to do so. The
    # `rust-src` component is required in order for this to work.
    RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
  };

  # Development tools
  nativeBuildInputs = [
    # Always use nightly rustfmt because most of its options are unstable
    #
    # This needs to come before `toolchain` in this list, otherwise
    # `$PATH` will have stable rustfmt instead.
    inputs.fenix.packages.${system}.latest.rustfmt

    # rust itself
    toolchain

    # CI tests
    engage

    # format toml files
    taplo

    # Needed for producing Debian packages
    cargo-deb

    # Needed for our script for Complement
    jq

    # Needed for Complement
    go
    olm

    # Needed for our script for Complement
    jq

    # Needed for finding broken markdown links
    lychee

    # Needed for checking for typos
    typos

    # Useful for editing the book locally
    mdbook
  ] ++ default.nativeBuildInputs ;
}
