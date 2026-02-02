# Keep sorted
{ default
, inputs
, lib
, mdbook
, stdenv
, xtask
}:

stdenv.mkDerivation {
  pname = "${default.pname}-book";
  version = default.version;


  src = let filter = inputs.nix-filter.lib; in filter {
    root = inputs.self;

    # Keep sorted
    include = [
      "book.toml"
      "conduit-example.toml"
      "debian/README.md"
      "docs"
      "README.md"
      "target/docs"
    ];
  };

  nativeBuildInputs = [
    mdbook
  ];

  buildPhase = ''
    ${lib.getExe xtask} generate-docs
    mdbook build
    mv public $out
  '';
}
