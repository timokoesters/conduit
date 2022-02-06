# ⚡️ Conduit

### An efficient Matrix homeserver written in Rust

## 🚀 Get started

You can find all the details on how to set up and manage your Conduit server in the **[Documentation](https://conduit-docs.surge.sh)**

## ❔ FAQ

<details open>
<summary>What is the goal?</summary>

An efficient Matrix homeserver that's easy to set up and just works. You can install
it on a mini-computer like the Raspberry Pi to host Matrix for your family,
friends or company.

</details>

<details>
<summary>Can I try it out?</summary>

Yes! You can test our Conduit instance by opening a Matrix client (<https://app.element.io> or Element Android for
example) and registering on the `conduit.rs` homeserver.

It is hosted on a ODROID HC 2 with 2GB RAM and a SAMSUNG Exynos 5422 CPU, which
was used in the Samsung Galaxy S5. It joined many big rooms, including Matrix
HQ.

</details>

<details>
<summary>What is the current status?</summary>

Conduit is Beta, meaning you can join and participate in most
Matrix rooms, but not all features are supported, and you might run into bugs
from time to time.

There are still a few important features missing:

- E2EE verification over federation
- Outgoing read receipts, typing, presence over federation

Check out the [Conduit 1.0 Release Milestone](https://gitlab.com/famedly/conduit/-/milestones/3).

</details>

## 💻 How to contribute

1. Look for an [issue](https://gitlab.com/famedly/conduit/-/issues) you would like to work on and make sure it's not assigned to other users
2. Ask someone to assign the issue to you (comment on the issue or chat in [#conduit:fachschaften.org](https://matrix.to/#/#conduit:fachschaften.org))
3. [Fork the repo](https://gitlab.com/famedly/conduit/-/forks/new) and work on the issue. [#conduit:fachschaften.org](https://matrix.to/#/#conduit:fachschaften.org) is happy to help :)
4. Submit a merge request

## 🤗 Thanks to

Thanks to [Famedly](https://famedly.com/), [Prototype Fund](https://prototypefund.de/) (DLR and German BMBF) and all other individuals for financially supporting this project.

Thanks to the contributors to Conduit and all libraries we use, for example:

- [Ruma](https://github.com/ruma/ruma): A clean library for the Matrix Spec in Rust
- [Axum](https://docs.rs/axum/latest/axum/): A modular web framework

## 💸 Donate

If you want to support the project, you can donate to Timo, the maintainer via [Liberapay](https://liberapay.com/timokoesters/) or Bitcoin (`bc1qnnykf986tw49ur7wx9rpw2tevpsztvar5x8w4n`)

## ⚡️ Logo

The Conduit Lightning Bolt logo is courtesy of [Mozilla FxEmojis](https://github.com/mozilla/fxemoji/blob/gh-pages/svgs/nature/u26A1-bolt.svg) ([CC BY 4.0](https://github.com/mozilla/fxemoji/blob/gh-pages/LICENSE.md))
