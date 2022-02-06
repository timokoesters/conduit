# Distribution Packages

> These packages are not maintained by the Conduit maaintainers. They are third-party community contributions we have no control over.

## Debian

[Paul](https://wiki.debian.org/PaulVanTilburg) has done work on preparing Conduit for Debian packaging. See the [Debian directory](https://gitlab.com/famedly/conduit/-/tree/next/debian) for more info about this.

```bash
# You'll need cargo-deb to create a debian package:
cargo install cargo-deb
# Run this in the Conduit repo to compile and create a package:
cargo deb
```

## NixOS 

[![nixpkgs unstable package](https://repology.org/badge/version-for-repo/nix_unstable/matrix-conduit.svg)](https://repology.org/project/matrix-conduit/versions)

[PimEyes](https://github.com/pimeys) has packaged
[Conduit for NixOS](https://search.nixos.org/packages?channel=unstable&show=matrix-conduit&from=0&size=50&sort=relevance&type=packages&query=matrix-conduit).

```bash
nix-env -iA nixos.matrix-conduit
```

## FreBSD Ports

[![FreeBSD port](https://repology.org/badge/version-for-repo/freebsd/matrix-conduit.svg)](https://repology.org/project/matrix-conduit/versions)

Apparently, there is also a [FreeBSD Port of Conduit](https://www.freshports.org/net-im/conduit).

```bash
cd /usr/ports/net-im/conduit/ && make install clean
```

## Void Linux

[![Void Linux x86_64 package](https://repology.org/badge/version-for-repo/void_x86_64/matrix-conduit.svg)](https://repology.org/project/matrix-conduit/versions)

[Joel Beckmeyer](https://github.com/TinfoilSubmarine) carefully brought a [Void Linux package for Conduit](https://github.com/void-linux/void-packages/blob/master/srcpkgs/conduit/template) to life.

```bash
xbps-install -S conduit
```