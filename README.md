# Conduit
### A Matrix homeserver written in Rust

#### What is the goal?

An efficient Matrix homeserver that's easy to set up and just works. You can install
it on a mini-computer like the Raspberry Pi to host Matrix for your family,
friends or company.


#### Can I try it out?

Yes! You can test our Conduit instance by opening a Matrix client (<https://app.element.io> or Element Android for
example) and registering on the `conduit.rs` homeserver.

It is hosted on a ODROID HC 2 with 2GB RAM and a SAMSUNG Exynos 5422 CPU, which
was used in the Samsung Galaxy S5. It joined many big rooms including Matrix
HQ.


#### What is the current status?

As of 2021-09-01, Conduit is Beta, meaning you can join and participate in most
Matrix rooms, but not all features are supported and you might run into bugs
from time to time.

There are still a few important features missing:

- E2EE verification over federation
- Outgoing read receipts, typing, presence over federation

Check out the [Conduit 1.0 Release Milestone](https://gitlab.com/famedly/conduit/-/milestones/3).


#### How can I deploy my own?

Simple install (this was tested the most): [DEPLOY.md](DEPLOY.md)\
Debian package: [debian/README.Debian](debian/README.Debian)\
Docker: [docker/README.md](docker/README.md)

If you want to connect an Appservice to Conduit, take a look at [APPSERVICES.md](APPSERVICES.md).


#### How can I contribute?

1. Look for an issue you would like to work on and make sure it's not assigned
   to other users
2. Ask someone to assign the issue to you (comment on the issue or chat in
   #conduit:nordgedanken.dev)
3. Fork the repo and work on the issue. #conduit:nordgedanken.dev is happy to help :)
4. Submit a MR


#### Thanks to

Thanks to Famedly, Prototype Fund (DLR and German BMBF) and all other individuals for financially supporting this project.

Thanks to the contributors to Conduit and all libraries we use, for example:

- Ruma: A clean library for the Matrix Spec in Rust
- Rocket: A flexible web framework


#### Donate

Liberapay: <https://liberapay.com/timokoesters/>\
Bitcoin: `bc1qnnykf986tw49ur7wx9rpw2tevpsztvar5x8w4n`


#### Logo

Lightning Bolt Logo: https://github.com/mozilla/fxemoji/blob/gh-pages/svgs/nature/u26A1-bolt.svg \
Logo License: https://github.com/mozilla/fxemoji/blob/gh-pages/LICENSE.md
