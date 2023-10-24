# Deploying Conduit

> ## Getting help
>
> If you run into any problems while setting up Conduit, write an email to `conduit@koesters.xyz`, ask us
> in `#conduit:fachschaften.org` or [open an issue on GitLab](https://gitlab.com/famedly/conduit/-/issues/new).

## Installing Conduit

Although you might be able to compile Conduit for Windows, we do recommend running it on a Linux server. We therefore
only offer Linux binaries.

You may simply download the binary that fits your machine. Run `uname -m` to see what you need. Now copy the appropriate url:

**Stable versions:**

| CPU Architecture                            | Download stable version                                         |
| ------------------------------------------- | --------------------------------------------------------------- |
| x84_64 / amd64 (Most servers and computers) | [Binary][x84_64-glibc-master] / [.deb][x84_64-glibc-master-deb] |
| armv7 (e.g. Raspberry Pi by default)        | [Binary][armv7-glibc-master] / [.deb][armv7-glibc-master-deb]   |
| armv8 / aarch64                             | [Binary][armv8-glibc-master] / [.deb][armv8-glibc-master-deb]   |

These builds were created on and linked against the glibc version shipped with Debian bullseye.
If you use a system with an older glibc version (e.g. RHEL8), you might need to compile Conduit yourself.

[x84_64-glibc-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/build-output/linux_amd64/conduit?job=docker:master
[armv7-glibc-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/build-output/linux_arm_v7/conduit?job=docker:master
[armv8-glibc-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/build-output/linux_arm64/conduit?job=docker:master
[x84_64-glibc-master-deb]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/build-output/linux_amd64/conduit.deb?job=docker:master
[armv7-glibc-master-deb]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/build-output/linux_arm_v7/conduit.deb?job=docker:master
[armv8-glibc-master-deb]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/build-output/linux_arm64/conduit.deb?job=docker:master

**Latest versions:**

| Target | Type | Download |
|-|-|-|
| `x86_64-unknown-linux-gnu` | Dynamically linked Debian package | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/conduit.deb?job=debian:x86_64-unknown-linux-gnu) |
| `x86_64-unknown-linux-musl` | Statically linked binary | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/conduit?job=static:x86_64-unknown-linux-musl) |
| `aarch64-unknown-linux-musl` | Statically linked binary | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/conduit?job=static:aarch64-unknown-linux-musl) |
| `x86_64-unknown-linux-musl` | OCI image | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/oci-image-amd64.tar.gz?job=oci-image:x86_64-unknown-linux-musl) |
| `aarch64-unknown-linux-musl` | OCI image | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/oci-image-arm64v8.tar.gz?job=oci-image:aarch64-unknown-linux-musl) |

```bash
$ sudo wget -O /usr/local/bin/matrix-conduit <url>
$ sudo chmod +x /usr/local/bin/matrix-conduit
```

Alternatively, you may compile the binary yourself. First, install any dependencies:

```bash
# Debian
$ sudo apt install libclang-dev build-essential

# RHEL
$ sudo dnf install clang
```
Then, `cd` into the source tree of conduit-next and run:
```bash
$ cargo build --release
```

If you want to cross compile Conduit to another architecture, read the guide below.

<details>
<summary>Cross compilation</summary>

As easiest way to compile conduit for another platform [cross-rs](https://github.com/cross-rs/cross) is recommended, so install it first.

In order to use RockDB as storage backend append `-latomic` to linker flags.

For example, to build a binary for Raspberry Pi Zero W (ARMv6) you need `arm-unknown-linux-gnueabihf` as compilation
target.

```bash
git clone https://gitlab.com/famedly/conduit.git
cd conduit
export RUSTFLAGS='-C link-arg=-lgcc -Clink-arg=-latomic -Clink-arg=-static-libgcc'
cross build --release --no-default-features --features conduit_bin,backend_rocksdb,jemalloc --target=arm-unknown-linux-gnueabihf
```
</details>

## Adding a Conduit user

While Conduit can run as any user it is usually better to use dedicated users for different services. This also allows
you to make sure that the file permissions are correctly set up.

In Debian or RHEL, you can use this command to create a Conduit user:

```bash
sudo adduser --system conduit --group --disabled-login --no-create-home
```

## Forwarding ports in the firewall or the router

Conduit uses the ports 443 and 8448 both of which need to be open in the firewall.

If Conduit runs behind a router or in a container and has a different public IP address than the host system these public ports need to be forwarded directly or indirectly to the port mentioned in the config.

## Optional: Avoid port 8448

If Conduit runs behind Cloudflare reverse proxy, which doesn't support port 8448 on free plans, [delegation](https://matrix-org.github.io/synapse/latest/delegate.html) can be set up to have federation traffic routed to port 443:
```apache
# .well-known delegation on Apache
<Files "/.well-known/matrix/server">
    ErrorDocument 200 '{"m.server": "your.server.name:443"}'
    Header always set Content-Type application/json
    Header always set Access-Control-Allow-Origin *
</Files>
```
[SRV DNS record](https://spec.matrix.org/latest/server-server-api/#resolving-server-names) delegation is also [possible](https://www.cloudflare.com/en-gb/learning/dns/dns-records/dns-srv-record/).

## Setting up a systemd service

Now we'll set up a systemd service for Conduit, so it's easy to start/stop Conduit and set it to autostart when your
server reboots. Simply paste the default systemd service you can find below into
`/etc/systemd/system/conduit.service`.

```systemd
[Unit]
Description=Conduit Matrix Server
After=network.target

[Service]
Environment="CONDUIT_CONFIG=/etc/matrix-conduit/conduit.toml"
User=conduit
Group=conduit
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

Now we need to create the Conduit's config file in `/etc/matrix-conduit/conduit.toml`. Paste this in **and take a moment
to read it. You need to change at least the server name.**  
You can also choose to use a different database backend, but right now only `rocksdb` and `sqlite` are recommended.

```toml
[global]
# The server_name is the pretty name of this server. It is used as a suffix for user
# and room ids. Examples: matrix.org, conduit.rs

# The Conduit server needs all /_matrix/ requests to be reachable at
# https://your.server.name/ on port 443 (client-server) and 8448 (federation).

# If that's not possible for you, you can create /.well-known files to redirect
# requests. See
# https://matrix.org/docs/spec/client_server/latest#get-well-known-matrix-client
# and
# https://matrix.org/docs/spec/server_server/r0.1.4#get-well-known-matrix-server
# for more information

# YOU NEED TO EDIT THIS
#server_name = "your.server.name"

# This is the only directory where Conduit will save its data
database_path = "/var/lib/matrix-conduit/"
database_backend = "rocksdb"

# The port Conduit will be running on. You need to set up a reverse proxy in
# your web server (e.g. apache or nginx), so all requests to /_matrix on port
# 443 and 8448 will be forwarded to the Conduit instance running on this port
# Docker users: Don't change this, you'll need to map an external port to this.
port = 6167

# Max size for uploads
max_request_size = 20_000_000 # in bytes

# Enables registration. If set to false, no users can register on this server.
allow_registration = true

allow_federation = true
allow_check_for_updates = true

# Server to get public keys from. You probably shouldn't change this
trusted_servers = ["matrix.org"]

#max_concurrent_requests = 100 # How many requests Conduit sends to other servers at the same time
#log = "warn,state_res=warn,rocket=off,_=off,sled=off"

address = "127.0.0.1" # This makes sure Conduit can only be reached using the reverse proxy
#address = "0.0.0.0" # If Conduit is running in a container, make sure the reverse proxy (ie. Traefik) can reach it.
```

## Setting the correct file permissions

As we are using a Conduit specific user we need to allow it to read the config. To do that you can run this command on
Debian or RHEL:

```bash
sudo chown -R root:root /etc/matrix-conduit
sudo chmod 755 /etc/matrix-conduit
```

If you use the default database path you also need to run this:

```bash
sudo mkdir -p /var/lib/matrix-conduit/
sudo chown -R conduit:conduit /var/lib/matrix-conduit/
sudo chmod 700 /var/lib/matrix-conduit/
```

## Setting up the Reverse Proxy

This depends on whether you use Apache, Caddy, Nginx or another web server.

### Apache

Create `/etc/apache2/sites-enabled/050-conduit.conf` and copy-and-paste this:

```apache
# Requires mod_proxy and mod_proxy_http
#
# On Apache instance compiled from source,
# paste into httpd-ssl.conf or httpd.conf

Listen 8448

<VirtualHost *:443 *:8448>

ServerName your.server.name # EDIT THIS

AllowEncodedSlashes NoDecode
ProxyPass /_matrix/ http://127.0.0.1:6167/_matrix/ timeout=300 nocanon
ProxyPassReverse /_matrix/ http://127.0.0.1:6167/_matrix/

</VirtualHost>
```

**You need to make some edits again.** When you are done, run

```bash
# Debian
$ sudo systemctl reload apache2

# Installed from source
$ sudo apachectl -k graceful
```

### Caddy

Create `/etc/caddy/conf.d/conduit_caddyfile` and enter this (substitute for your server name).

```caddy
your.server.name, your.server.name:8448 {
        reverse_proxy /_matrix/* 127.0.0.1:6167
}
```

That's it! Just start or enable the service and you're set.

```bash
$ sudo systemctl enable caddy
```

### Nginx

If you use Nginx and not Apache, add the following server section inside the http section of `/etc/nginx/nginx.conf`

```nginx
server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    listen 8448 ssl http2;
    listen [::]:8448 ssl http2;
    server_name your.server.name; # EDIT THIS
    merge_slashes off;

    # Nginx defaults to only allow 1MB uploads
    # Increase this to allow posting large files such as videos
    client_max_body_size 20M;

    location /_matrix/ {
        proxy_pass http://127.0.0.1:6167;
        proxy_set_header Host $http_host;
        proxy_buffering off;
        proxy_read_timeout 5m;
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

If you chose Caddy as your web proxy SSL certificates are handled automatically and you can skip this step.

The easiest way to get an SSL certificate, if you don't have one already, is to [install](https://certbot.eff.org/instructions) `certbot` and run this:

```bash
# To use ECC for the private key, 
# paste into /etc/letsencrypt/cli.ini:
# key-type = ecdsa
# elliptic-curve = secp384r1

$ sudo certbot -d your.server.name
```
[Automated renewal](https://eff-certbot.readthedocs.io/en/stable/using.html#automated-renewals) is usually preconfigured.

If using Cloudflare, configure instead the edge and origin certificates in dashboard. In case you’re already running a website on the same Apache server, you can just copy-and-paste the SSL configuration from your main virtual host on port 443 into the above-mentioned vhost.

## You're done!

Now you can start Conduit with:

```bash
$ sudo systemctl start conduit
```

Set it to start automatically when your system boots with:

```bash
$ sudo systemctl enable conduit
```

## How do I know it works?

You can open <https://app.element.io>, enter your homeserver and try to register.

You can also use these commands as a quick health check.

```bash
$ curl https://your.server.name/_matrix/client/versions

# If using port 8448
$ curl https://your.server.name:8448/_matrix/client/versions
```

- To check if your server can talk with other homeservers, you can use the [Matrix Federation Tester](https://federationtester.matrix.org/).
  If you can register but cannot join federated rooms check your config again and also check if the port 8448 is open and forwarded correctly.

# What's next?

## Audio/Video calls

For Audio/Video call functionality see the [TURN Guide](TURN.md).

## Appservices

If you want to set up an appservice, take a look at the [Appservice Guide](APPSERVICES.md).
