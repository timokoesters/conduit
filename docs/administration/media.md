# Media

While running Conduit, you may encounter undesirable media, either from other servers, or from local users.

## From other servers
If the media originated from a different server, which itself is not malicious, it should be enough
to use the `purge-media-from-server` command to delete the media from the media backend, and then
contact the remote server so that they can deal with the offending user(s).

If you do not need to media deleted as soon as possible, you can use retention policies to only
store remote media for a short period of time, meaning that the media will be automatically deleted
after some time. As new media can only be accessed over authenticated endpoints, only local users
will be able to access the media via your server, so if you're running a single-user server, you
don't need to worry about the media being distributed via your server.

If you know the media IDs, (which you can find with the `list-media` command), you can use the
`block-media` to prevent any of those media IDs (or other media with the same SHA256 hash) from
being stored in the media backend in the future.

If the server itself if malicious, then it should probably be [ACLed](https://spec.matrix.org/v1.14/client-server-api/#server-access-control-lists-acls-for-rooms)
in rooms it particpates in. In the future, you'll be able to block the remote server from
interacting with your server completely.

## From local users
If the undesirable media originates from your own server, you can purge media uploaded by them
using the `purge-media-from-users` command. If you also plan to deactivate the user, you can do so
with the `--purge-media` flag on either the `deactivate-user` or `deactivate-all` commands. If
they keep making new accounts, you can use the `block-media-from-users` command to prevent media
with the same SHA256 hash from being uploaded again, as well as using the `allow-registration`
command to temporarily prevent users from creating new accounts.
