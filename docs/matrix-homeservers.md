# About Matrix Homeservers

Matrix homeservers manage its users chats. Every Matrix username includes the homserver it belongs to:
`@alice:matrix.org` means that the `matrix.org` homeserver hosts a user called `@alice`.
Every time someone chats with Alice, the `matrix.org` homeserver stores these messages.
When `@alice:matrix.org` talks with `@adelaide:matrix.org`, that's easy. Both users use the same server.

But how can `@bob:vector.tld`, who uses the `vector.tld` homeserver, exchange messages with `@alice:matrix.org`?
This is where it get's a bit more complicated.

## Matrix Homeserver discovery

The Matrix specification specifies multiple ways how servers can discover and then talk to each other.
Let's look at the most common one:

### .well-known files

At first, the only information a server has about a user (e.g. `@bob:vector.tld`) is its homeserver name: `vector.tld`.
It then makes a HTTP GET request to `https://vector.tld/.well-known/matrix/server`.
In the ideal case, this file contains a content like this: 

```json
{
  "m.server": "matrix.vector.tld:443"
}
```

This translates to: The matrix homeserver software for users with a username ending on `vector.tld`
can be found at the address `matrix.vector.tld` at port 443 (which is the common port for HTTPS).

The homeserver on it's quest to find `@bob:vector.tld` now contacts `matrix.vector.tld:443` and is then
able to exchange chat messages with it.


### Why so complicated?

Organizations often don't want to run their Matrix server on the same machine that hosts their website,
but `@foo:matrix.evil.corp` usernames are ugly and everyone wants to be `@foo:evil.corp`.

To solve that problem, Matrix implements this extra step via a .well-known file or a DNS entry.