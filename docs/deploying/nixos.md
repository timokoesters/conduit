# Conduit for NixOS

Conduit can be acquired by Nix from various places:

* The `flake.nix` at the root of the repo
* The `default.nix` at the root of the repo
* From Nixpkgs

The `flake.nix` and `default.nix` do not (currently) provide a NixOS module, so
(for now) [`services.matrix-conduit`][module] from Nixpkgs should be used to
configure Conduit.

If you want to run the latest code, you should get Conduit from the `flake.nix`
or `default.nix` and set [`services.matrix-conduit.package`][package]
appropriately.

[module]: https://search.nixos.org/options?channel=unstable&query=services.matrix-conduit
[package]: https://search.nixos.org/options?channel=unstable&query=services.matrix-conduit.package
