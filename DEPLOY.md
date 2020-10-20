# Deploy from source

## Prerequisites

Make sure you have `libssl-dev` and `pkg-config` installed and the [rust toolchain](https://rustup.rs) is available on at least on user.


## Install Conduit

You have to download the binary that fits your machine. Run `uname -m` to see what you need:
- x84_64: `https://conduit.rs/master/x86_64/conduit-bin`
- armv7: `https://conduit.rs/master/armv7/conduit-bin`
- armv8: `https://conduit.rs/master/armv8/conduit-bin`
- arm: `https://conduit.rs/master/arm/conduit-bin`

```bash
$ sudo useradd -m conduit
$ sudo -u conduit wget <url> -O /home/conduit/conduit-bin && chmod +x /home/conduit/conduit-bin
```


## Setup systemd service

In this guide, we set up a systemd service for Conduit, so it's easy to
start/stop Conduit and set it to autostart when your server reboots. Paste the
default systemd service you can find below into
`/etc/systemd/system/conduit.service` and configure it to fit your setup.

```systemd
[Unit]
Description=Conduit
After=network.target

[Service]
Environment="ROCKET_SERVER_NAME=YOURSERVERNAME.HERE" # EDIT THIS

Environment="ROCKET_PORT=14004" # Reverse proxy port

#Environment="ROCKET_MAX_REQUEST_SIZE=20000000" # in bytes
#Environment="ROCKET_REGISTRATION_DISABLED=true"
#Environment="ROCKET_ENCRYPTION_DISABLED=true"
#Environment="ROCKET_FEDERATION_ENABLED=true"
#Environment="ROCKET_LOG=normal" # Detailed logging

Environment="ROCKET_ENV=production"
User=conduit
Group=conduit
Type=simple
Restart=always
ExecStart=/home/conduit/conduit-bin

[Install]
WantedBy=multi-user.target
```

Finally, run
```bash
$ sudo systemctl daemon-reload
```


## Setup Reverse Proxy

This depends on whether you use Apache, Nginx or something else. For Apache it looks like this (in /etc/apache2/sites-enabled/050-conduit.conf):
```
<VirtualHost *:443>

ServerName conduit.koesters.xyz # EDIT THIS

AllowEncodedSlashes NoDecode

ServerAlias conduit.koesters.xyz # EDIT THIS

ProxyPreserveHost On
ProxyRequests off
AllowEncodedSlashes NoDecode
ProxyPass / http://localhost:14004/ nocanon
ProxyPassReverse / http://localhost:14004/ nocanon

Include /etc/letsencrypt/options-ssl-apache.conf

# EDIT THESE:
SSLCertificateFile /etc/letsencrypt/live/conduit.koesters.xyz/fullchain.pem
SSLCertificateKeyFile /etc/letsencrypt/live/conduit.koesters.xyz/privkey.pem
</VirtualHost>
```

Then run
```bash
$ sudo systemctl reload apache2
```


## SSL Certificate

The easiest way to get an SSL certificate for the domain is to install `certbot` and run this:
```bash
$ sudo certbot -d conduit.koesters.xyz
```


## You're done!

Now you can start Conduit with:
```bash
$ sudo systemctl start conduit
```

Set it to start automatically when your system boots with:
```bash
$ sudo systemctl enable conduit
```
