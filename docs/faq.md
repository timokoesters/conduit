# FAQ

Here are some of the most frequently asked questions about Conduit, and their answers.

## Why do I get a `M_INCOMPATIBLE_ROOM_VERSION` error when trying to join some rooms?

Conduit doesn't support room versions 1 and 2 at all, and doesn't properly support versions 3-5 currently. You can track the progress of adding support [here](https://gitlab.com/famedly/conduit/-/issues/433).

## How do I setup sliding sync?

You need to add a `org.matrix.msc3575.proxy` field to your `.well-known/matrix/client` response which points to Conduit. Here is an example:
```json
{
  "m.homeserver": {
    "base_url": "https://matrix.example.org"
  },
  "org.matrix.msc3575.proxy": {
    "url": "https://matrix.example.org"
  }
}
```

## Can I migrate from Synapse to Conduit?

Not really. You can reuse the domain of your current server with Conduit, but you will not be able to migrate accounts automatically.
Rooms that were federated can be re-joined via the other participating servers, however media and the like may be deleted from remote servers after some time, and hence might not be recoverable.
