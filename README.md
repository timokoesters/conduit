# Conduit

<!-- ANCHOR: catchphrase -->
### A Matrix homeserver written in Rust
<!-- ANCHOR_END: catchphrase -->

Please visit the [Conduit documentation](https://famedly.gitlab.io/conduit) for more information.
Alternatively you can open [docs/introduction.md](docs/introduction.md) in this repository.

<!-- ANCHOR: body -->
#### What is Matrix?

[Matrix](https://matrix.org) is an open network for secure and decentralized
communication. Users from every Matrix homeserver can chat with users from all
other Matrix servers. You can even use bridges (also called Matrix appservices)
to communicate with users outside of Matrix, like a community on Discord.

#### What is the goal?

An efficient Matrix homeserver that's easy to set up and just works. You can install
it on a mini-computer like the Raspberry Pi to host Matrix for your family,
friends or company.

#### Can I try it out?

Yes! You can test our Conduit instance by opening a client that supports registration tokens such as [Element web](https://app.element.io/), [Nheko](https://matrix.org/ecosystem/clients/nheko/) or [SchildiChat web](https://app.schildi.chat/) and registering on the `conduit.rs` homeserver. The registration token is "for_testing_only". Don't share personal information. Once you have registered, you can use any other [Matrix client](https://matrix.org/ecosystem/clients) to login.

Server hosting for conduit.rs is donated by the Matrix.org Foundation.

#### What is the current status?

Conduit is Beta, meaning you can join and participate in most
Matrix rooms, but not all features are supported and you might run into bugs
from time to time.

There are still a few important features missing:

- E2EE emoji comparison over federation (E2EE chat works)
- Outgoing read receipts, typing, presence over federation (incoming works)
<!-- ANCHOR_END: body -->

<!-- ANCHOR: footer -->
#### How can I contribute?

1. Look for an issue you would like to work on and make sure no one else is currently working on it.
2. Tell us that you are working on the issue (comment on the issue or chat in
   [#conduit:fachschaften.org](https://matrix.to/#/#conduit:fachschaften.org)). If it is more complicated, please explain your approach and ask questions.
3. Fork the repo, create a new branch and push commits.
4. Submit a MR

#### Contact

If you have any questions, feel free to
- Ask in `#conduit:fachschaften.org` on Matrix
- Write an E-Mail to `conduit@koesters.xyz`
- Send an direct message to `@timokoesters:fachschaften.org` on Matrix
- [Open an issue on GitLab](https://gitlab.com/famedly/conduit/-/issues/new)

#### Security

If you believe you have found a security issue, please send a message to [Timo](https://matrix.to/#/@timo:conduit.rs)
and/or [Matthias](https://matrix.to/#/@matthias:ahouansou.cz) on Matrix, or send an email to
[conduit@koesters.xyz](mailto:conduit@koesters.xyz). Please do not disclose details about the issue to anyone else before
a fix is released publically.

#### Thanks to

Thanks to FUTO, Famedly, Prototype Fund (DLR and German BMBF) and all individuals for financially supporting this project.

Thanks to the contributors to Conduit and all libraries we use, for example:

- Ruma: A clean library for the Matrix Spec in Rust
- axum: A modular web framework

#### Donate

- Liberapay: <https://liberapay.com/timokoesters/>
- Bitcoin: `bc1qnnykf986tw49ur7wx9rpw2tevpsztvar5x8w4n`

#### Logo

- Lightning Bolt Logo: <https://github.com/mozilla/fxemoji/blob/gh-pages/svgs/nature/u26A1-bolt.svg>
- Logo License: <https://github.com/mozilla/fxemoji/blob/gh-pages/LICENSE.md>
<!-- ANCHOR_END: footer -->
