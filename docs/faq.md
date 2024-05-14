# FAQ

Here are some of the most frequently asked questions about Conduit, and their answers.

## Why do I get a `M_INCOMPATIBLE_ROOM_VERSION` error when trying to join some rooms?

Conduit doesn't support room versions 1 and 2 at all, and doesn't properly support versions 3-5 currently. You can track the progress of adding support [here](https://gitlab.com/famedly/conduit/-/issues/433).

## How do I backup my server?

To backup your Conduit server, it's very easy.
You can simply stop Conduit, make a copy or file system snapshot of the database directory, then start Conduit again.

> **Note**: When using a file system snapshot, it is not required that you stop the server, but it is still recommended as it is the safest option and should ensure your database is not left in an inconsistent state.

## How do I setup sliding sync?

If you use the [automatic method for delegation](delegation.md#automatic-recommended) or just proxy `.well-known/matrix/client` to Conduit, sliding sync should work with no extra configuration.
If you don't, continue below.

You need to add a `org.matrix.msc3575.proxy` field to your `.well-known/matrix/client` response which contains a url which Conduit is accessible behind.
Here is an example:
```json
{
~  "m.homeserver": {
~    "base_url": "https://matrix.example.org"
~  },
  "org.matrix.msc3575.proxy": {
    "url": "https://matrix.example.org"
  }
}
```

## Can I migrate from Synapse to Conduit?

Not really. You can reuse the domain of your current server with Conduit, but you will not be able to migrate accounts automatically.
Rooms that were federated can be re-joined via the other participating servers, however media and the like may be deleted from remote servers after some time, and hence might not be recoverable.

## How do I make someone an admin?

Simply invite them to the admin room. Once joined, they can administer the server by interacting with the `@conduit:<server_name>` user.
