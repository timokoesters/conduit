# Distribution packages

## Debian / Ubuntu

[@paul:luon.net](https://matrix.to/#/@paul:luon.net) plans to package Conduit for Debian as soon as it reaches 1.0.
Until it is available in the official repos, you can install the development version of it manually:

```bash
sudo apt-get install ca-certificates
wget --https-only -O /tmp/conduit.deb https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/conduit-x86_64-unknown-linux-gnu.deb?job=build:cargo-deb:x86_64-unknown-linux-gnu
sudo dpkg -i /tmp/conduit.deb
```
