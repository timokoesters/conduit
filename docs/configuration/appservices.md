# Setting up Appservices

## Getting help

If you run into any problems while setting up an Appservice, write an email to `timo@koesters.xyz`, ask us in `#conduit:matrix.org` or [open an issue on GitLab](https://gitlab.com/famedly/conduit/-/issues/new).

## Tested appservices

Here are some appservices we tested and that work with Conduit:
- [matrix-appservice-discord](https://github.com/Half-Shot/matrix-appservice-discord)
- [mautrix-hangouts](https://github.com/mautrix/hangouts/)
- [mautrix-telegram](https://github.com/mautrix/telegram/)
- [mautrix-signal](https://github.com/mautrix/signal)
  - There are a few things you need to do, in order for the bridge (at least up to version `0.2.0`) to work. Before following the bridge installation guide, you need to map apply a patch to bridges `portal.py`. Go to [portal.py](https://github.com/mautrix/signal/blob/master/mautrix_signal/portal.py) at [mautrix-signal](https://github.com/mautrix/signal) (don't forget to change to the correct commit/version of the file) and copy its content, create a `portal.py` on your host system and paste it in. Now you need to change two lines:
  [Line 1020](https://github.com/mautrix/signal/blob/4ea831536f154aba6419d13292479eb383ea3308/mautrix_signal/portal.py#L1020)

    ```diff
    --- levels.users[self.main_intent.mxid] = 9001 if is_initial else 100
    +++ levels.users[self.main_intent.mxid] = 100 if is_initial else 100
    ```

    and add a new line between [Lines 1041 and 1042](https://github.com/mautrix/signal/blob/4ea831536f154aba6419d13292479eb383ea3308/mautrix_signal/portal.py#L1041-L1042)

    ```diff
        "type": str(EventType.ROOM_POWER_LEVELS),
    +++ "state_key": "",
        "content": power_levels.serialize(),
    ```

    Now you just need to map the patched `portal.py` into the `mautrix-signal` container
    ```yml
    volumes:
      - ./<your>/<path>/<on>/<host>/portal.py:/usr/lib/python3.9/site-packages/mautrix_signal/portal.py
    ```
    and then read below and start following the bridge [installation instructions](https://docs.mau.fi/bridges/index.html).

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
