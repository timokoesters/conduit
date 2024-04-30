# Keep sorted
{ default
, inputs
, mdbook
, stdenv
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
    ];
  };

  nativeBuildInputs = [
    mdbook
  ];

  buildPhase = ''
    mdbook build
    mv public $out
  '';
}
