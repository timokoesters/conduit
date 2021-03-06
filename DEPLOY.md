# Deploying Conduit

## Getting help

If you run into any problems while setting up Conduit, write an email to `timo@koesters.xyz`, ask us in `#conduit:matrix.org` or [open an issue on GitLab](https://gitlab.com/famedly/conduit/-/issues/new).

## Installing Conduit

You have to download the binary that fits your machine. Run `uname -m` to see
what you need. Now copy the right url:
- x84_64: `https://conduit.rs/master/x86_64/conduit-bin`
- armv7: `https://conduit.rs/master/armv7/conduit-bin`
- armv8: `https://conduit.rs/master/armv8/conduit-bin`
- arm: `https://conduit.rs/master/arm/conduit-bin`

```bash
$ sudo wget -O /usr/local/bin/matrix-conduit <url>
$ sudo chmod +x /usr/local/bin/matrix-conduit
```

## Adding a Conduit user

While Conduit can run as any user it is usually better to use dedicated users for different services.
This also allows you to make sure that the file permissions are correctly set up.

In Debian you can use this command to create a Conduit user:

```
sudo adduser --system conduit --no-create-home
```

## Setting up a systemd service

Now we'll set up a systemd service for Conduit, so it's easy to start/stop
Conduit and set it to autostart when your server reboots. Simply paste the
default systemd service you can find below into
`/etc/systemd/system/conduit.service`.

```systemd
[Unit]
Description=Conduit Matrix Server
After=network.target

[Service]
Environment="CONDUIT_CONFIG=/etc/matrix-conduit/conduit.toml"
User=conduit
Group=nogroup
Restart=always
ExecStart=/usr/local/bin/matrix-conduit

[Install]
WantedBy=multi-user.target
```

Finally, run

```bash
$ sudo systemctl daemon-reload
```


## Creating the Conduit configuration file

Now we need to create the Conduit's config file in `/etc/matrix-conduit/conduit.toml`. Paste this in **and take a moment to read it. You need to change at least the server name.**

```toml
[global]
# The server_name is the name of this server. It is used as a suffix for user
# and room ids. Examples: matrix.org, conduit.rs
# The Conduit server needs to be reachable at https://your.server.name/ on port
# 443 (client-server) and 8448 (federation) OR you can create /.well-known
# files to redirect requests. See
# https://matrix.org/docs/spec/client_server/latest#get-well-known-matrix-client
# and https://matrix.org/docs/spec/server_server/r0.1.4#get-well-known-matrix-server
# for more information

# YOU NEED TO EDIT THIS
#server_name = "your.server.name"

# This is the only directory where Conduit will save its data
database_path = "/var/lib/matrix-conduit/conduit_db"

# The port Conduit will be running on. You need to set up a reverse proxy in
# your web server (e.g. apache or nginx), so all requests to /_matrix on port
# 443 and 8448 will be forwarded to the Conduit instance running on this port
port = 6167

# Max size for uploads
max_request_size = 20_000_000 # in bytes

# Disabling registration means no new users will be able to register on this server
allow_registration = false

# Disable encryption, so no new encrypted rooms can be created
# Note: existing rooms will continue to work
allow_encryption = true
allow_federation = true

trusted_servers = ["matrix.org"]

#cache_capacity = 1073741824 # in bytes, 1024 * 1024 * 1024
#max_concurrent_requests = 100 # How many requests Conduit sends to other servers at the same time
#workers = 4 # default: cpu core count * 2

address = "127.0.0.1" # This makes sure Conduit can only be reached using the reverse proxy
```

## Setting the correct file permissions

As we are using a Conduit specific user we need to allow it to read the config.
To do that you can run this command on Debian:

```
sudo chown -R conduit:nogroup /etc/matrix-conduit
```

If you use the default database path you also need to run this:

```
sudo mkdir -p /var/lib/matrix-conduit/conduit_db
sudo chown -R conduit:nogroup /var/lib/matrix-conduit/conduit_db
```


## Setting up the Reverse Proxy

This depends on whether you use Apache, Nginx or another web server.

### Apache

Create `/etc/apache2/sites-enabled/050-conduit.conf` and copy-and-paste this:

```
Listen 8448

<VirtualHost *:443 *:8448>

ServerName your.server.name # EDIT THIS

AllowEncodedSlashes NoDecode
ProxyPass /_matrix/ http://127.0.0.1:6167/_matrix/ nocanon
ProxyPassReverse /_matrix/ http://127.0.0.1:6167/_matrix/

Include /etc/letsencrypt/options-ssl-apache.conf
SSLCertificateFile /etc/letsencrypt/live/your.server.name/fullchain.pem # EDIT THIS
SSLCertificateKeyFile /etc/letsencrypt/live/your.server.name/privkey.pem # EDIT THIS
</VirtualHost>
```

**You need to make some edits again.** When you are done, run

```bash
$ sudo systemctl reload apache2
```


### Nginx

If you use Nginx and not Apache, add the following server section inside the
http section of `/etc/nginx/nginx.conf`

```
server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    listen 8448 ssl http2;
    listen [::]:8448 ssl http2;
    server_name your.server.name; # EDIT THIS
    merge_slashes off;

    location /_matrix/ {
        proxy_pass http://127.0.0.1:6167$request_uri;
        proxy_set_header Host $http_host;
        proxy_buffering off;
    }

    ssl_certificate /etc/letsencrypt/live/your.server.name/fullchain.pem; # EDIT THIS
    ssl_certificate_key /etc/letsencrypt/live/your.server.name/privkey.pem; # EDIT THIS
    ssl_trusted_certificate /etc/letsencrypt/live/your.server.name/chain.pem; # EDIT THIS
    include /etc/letsencrypt/options-ssl-nginx.conf;
}
```
**You need to make some edits again.** When you are done, run

```bash
$ sudo systemctl reload nginx
```


## SSL Certificate

The easiest way to get an SSL certificate, if you don't have one already, is to install `certbot` and run this:

```bash
$ sudo certbot -d your.server.name
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

If you want to set up an appservice, take a look at the [Appservice Guide](APPSERVICES.md).
