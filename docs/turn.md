# Setting up TURN/STUN

## General instructions

* It is assumed you have a [Coturn server](https://github.com/coturn/coturn) up and running. See [Synapse reference implementation](https://github.com/element-hq/synapse/blob/develop/docs/turn-howto.md).

## Edit/Add a few settings to your existing conduit.toml

```
[turn]
# Refer to your Coturn settings. 
# `your.turn.url` has to match the REALM setting of your Coturn as well as `transport`.
uris = ["turn:your.turn.url?transport=udp", "turn:your.turn.url?transport=tcp"]

# static-auth-secret of your turnserver
secret = "ADD SECRET HERE"

# If you have your TURN server configured to use a username and password
# you can provide these information too. In this case comment out `turn_secret above`!
#username = ""
#password = ""
```

## Apply settings

Restart Conduit.
