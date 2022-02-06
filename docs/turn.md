# Setting up TURN/STURN

{{#include _getting_help.md}}

## General instructions

- It is assumed you have a [Coturn server](https://github.com/coturn/coturn) up and running. See [Synapse reference implementation](https://github.com/matrix-org/synapse/blob/e3fe6347be1da930b6a0ed2005b565369800a327/docs/turn-howto.md).

## Edit/Add a few settings to your existing conduit.toml

```toml
# Refer to your Coturn settings.
# `your.turn.url` has to match the REALM setting of your Coturn as well as `transport`.
turn_uris = ["turn:your.turn.url?transport=udp", "turn:your.turn.url?transport=tcp"]

# static-auth-secret of your turnserver
turn_secret = "ADD SECRET HERE"

# If you have your TURN server configured to use a username and password
# you can provide these information too. In this case comment out `turn_secret above`!
#turn_username = ""
#turn_password = ""
```

You can generate a secret by running the following command on Linux:

```bash
openssl rand -base64 33
```

## Apply settings

Restart Conduit.
