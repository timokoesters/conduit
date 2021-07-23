# Conduit

Conduit is a simple, fast and reliable chat server for the [Matrix] protocol written in [Rust].

-----
> Note: This project is work-in-progress. Do *not* rely on it yet.

## What is Matrix?

[Matrix] is an open network for secure and decentralized
communication. It allows you to chat with friends even if they are using
another servers and client. You can even use bridges to communicate with users
outside of Matrix, like a community on Discord or your family on Hangouts.

## Why Conduit?

Conduit is an open-source server implementation of the [Matrix
Specification] with a focus on easy setup and low
system requirements, making it very easy to set up.

Other server implementations try to be extremely scalable, which makes sense if
the goal is to support millions of users on a single instance, but makes
smaller deployments a lot more inefficient. Conduit tries to keep it simple but
takes full advantage of that, for example by using an in-memory database for
[huge performance gains](https://github.com/timokoesters/romeo-and-juliet-benchmark).

The future for Conduit in peer-to-peer Matrix (every client contains a server)
is also bright.

Conduit tries to be reliable by using the Rust programming language and paying
close attention to error handling to make sure that evil clients, misbehaving
servers or even a partially broken database will not cause the whole server to
stop working.

## Chat with us!

We have a room on Matrix: [#conduit:matrix.org](https://matrix.to/#/#conduit:matrix.org)

You can also contact us using:
- Matrix: [@timo:koesters.xyz](https://matrix.to/#/@timo:koesters.xyz)
- Email: [conduit@koesters.xyz](mailto:conduit@koesters.xyz)


## Donate

Liberapay: <https://liberapay.com/timokoesters/>\
Bitcoin: `bc1qnnykf986tw49ur7wx9rpw2tevpsztvar5x8w4n`


[Matrix]: https://matrix.org/
[Rust]: https://rust-lang.org
[Matrix Specification]: https://matrix.org/docs/spec