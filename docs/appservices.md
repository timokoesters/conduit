# Setting up Appservices

{{#include _getting_help.md}}

## Set up the appservice - general instructions

Follow whatever instructions are given by the appservice. This usually includes
downloading, changing its config (setting domain, homeserver url, port etc.)
and later starting it.

At some point the appservice guide should ask you to add a registration yaml
file to the homeserver. In Synapse you would do this by adding the path to the
homeserver.yaml, but in Conduit you can do this from within Matrix:

First, go into the #admins room of your homeserver. The first person that
registered on the homeserver automatically joins it. Then send a message into
the room like this:

    @conduit:your.server.name: register-appservice
    ```
    paste
    the
    contents
    of
    the
    yaml
    registration
    here
    ```

You can confirm it worked by sending a message like this:
`@conduit:your.server.name: list-appservices`

The @conduit bot should answer with `Appservices (1): your-bridge`

Then you are done. Conduit will send messages to the appservices and the
appservice can send requests to the homeserver. You don't need to restart
Conduit, but if it doesn't work, restarting while the appservice is running
could help.

## Appservice-specific instructions

### Remove an appservice

To remove an appservice go to your admin room and execute

`@conduit:your.server.name: unregister-appservice <name>`

where `<name>` one of the output of `list-appservices`.

### Tested appservices

These appservices have been tested and work with Conduit without any extra steps:

- [matrix-appservice-discord](https://github.com/Half-Shot/matrix-appservice-discord)
- [mautrix-hangouts](https://github.com/mautrix/hangouts/)
- [mautrix-telegram](https://github.com/mautrix/telegram/)
- [mautrix-signal](https://github.com/mautrix/signal/) from version `0.2.2` forward.
- [heisenbridge](https://github.com/hifi/heisenbridge/)
