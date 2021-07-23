# Setting up Appservices

## Getting help

If you run into any problems while setting up an Appservice, write an email to `timo@koesters.xyz`, ask us in `#conduit:matrix.org` or [open an issue on GitLab](https://gitlab.com/famedly/conduit/-/issues/new).

## Tested appservices

Here are some appservices we tested and that work with Conduit:

- matrix-appservice-discord
- mautrix-hangouts
- mautrix-telegram

## Set up the appservice

Follow whatever instructions are given by the appservice. This usually includes
downloading, changing its config (setting domain, homeserver url, port etc.)
and later starting it.

At some point the appservice guide should ask you to add a registration yaml
file to the homeserver. In Synapse you would do this by adding the path to the
homeserver.yaml, but in Conduit you can do this from within Matrix:

First, go into the #admins room of your homeserver. The first person that
registered on the homeserver automatically joins it. Then send a message into
the room like this:

    @conduit:your.server.name: register_appservice
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

`@conduit:your.server.name: list_appservices`

The @conduit bot should answer with `Appservices (1): your-bridge`

Then you are done. Conduit will send messages to the appservices and the
appservice can send requests to the homeserver. You don't need to restart
Conduit, but if it doesn't work, restarting while the appservice is running
could help.
