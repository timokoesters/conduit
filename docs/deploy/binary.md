## Installing Conduit with a binary

{{#include ../_getting_help.md}}

## Prerequisites

Although you might be able to compile Conduit for Windows, we do recommend running it on a Linux server.

This guide assumes you have root access to a Debian Linux server with at least 1 GB of available RAM and at least 10 GB of free disk space.
The more chats you join and the bigger these chats are, the more RAM and storage you'll need.

As Matrix uses HTTPS for communication, you'll also need a domain, like `matrix.org`. Whenever you see `your.server.name` in this guide, replace it with your actual domain.

## Download Conduit

You may simply download the binary that fits your machine. Run `uname -m` to see what you need. Now copy the right URL:

| CPU Architecture     | Download stable version        | Download development version |
| -------------------- | ------------------------------ | ---------------------------- |
| x84_64 / amd64       | [Download][x84_64-musl-master] | [Download][x84_64-musl-next] |
| armv6                | [Download][armv6-musl-master]  | [Download][armv6-musl-next]  |
| armv7 (Raspberry Pi) | [Download][armv7-musl-master]  | [Download][armv7-musl-next]  |
| armv8 / aarch64      | [Download][armv8-musl-master]  | [Download][armv8-musl-next]  |

[x84_64-musl-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/conduit-x86_64-unknown-linux-musl?job=build:release:cargo:x86_64-unknown-linux-musl
[armv6-musl-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/conduit-arm-unknown-linux-musleabihf?job=build:release:cargo:arm-unknown-linux-musleabihf
[armv7-musl-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/conduit-armv7-unknown-linux-musleabihf?job=build:release:cargo:armv7-unknown-linux-musleabihf
[armv8-musl-master]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/master/raw/conduit-aarch64-unknown-linux-musl?job=build:release:cargo:aarch64-unknown-linux-musl
[x84_64-musl-next]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/next/raw/conduit-x86_64-unknown-linux-musl?job=build:release:cargo:x86_64-unknown-linux-musl
[armv6-musl-next]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/next/raw/conduit-arm-unknown-linux-musleabihf?job=build:release:cargo:arm-unknown-linux-musleabihf
[armv7-musl-next]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/next/raw/conduit-armv7-unknown-linux-musleabihf?job=build:release:cargo:armv7-unknown-linux-musleabihf
[armv8-musl-next]: https://gitlab.com/famedly/conduit/-/jobs/artifacts/next/raw/conduit-aarch64-unknown-linux-musl?job=build:release:cargo:aarch64-unknown-linux-musl

```bash
sudo wget -O /usr/local/bin/matrix-conduit <url>
sudo chmod +x /usr/local/bin/matrix-conduit
```

## Or compile the binary yourself

If you don't want to use our prebuilt binaries, you can also compile Conduit yourself.

To do so, you'll need to install Rust and some dependencies:

```bash
sudo apt install git curl libclang-dev build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

If that succeeded, clone Conduit and build it:

```bash
git clone --depth 1 "https://gitlab.com/famedly/conduit.git" conduit && cd conduit
cargo build --release
sudo cp target/release/conduit /usr/local/bin/conduit
```

Note that this currently requires Rust 1.56, which should automatically be used when you installed Rust via rustup.

<details>
<summary>Cross-Compiling to different architectures</summary>

In theory, Rust offers smooth cross-compilation. But since Conduit is not pure-Rust (due to its database choices), you can't just `cargo build --target armv7-unknown-linux-musleabihf`.

But fear not, smart people (in this case, the wonderful [Maxim](@mdc:anter.io)) prepared some cross-images for you. So to cross-compile:

1. [Install Docker](https://docs.docker.com/get-docker/)
2. [Install cargo-cross](https://github.com/cross-rs/cross#installation)
3. Choose a target and compile with `cross build --target="YOUR_TARGET_HERE" --locked --release`

Currently supported targets are:

- `aarch64-unknown-linux-musl`
- `arm-unknown-linux-musleabihf`
- `armv7-unknown-linux-musleabihf`
- `x86_64-unknown-linux-musl`

</details>

## Adding a Conduit user

While Conduit can run as any user, it is usually better to use dedicated users for different services. This also allows
you to make sure that the file permissions are correctly set up.

In Debian, you can use this command to create a Conduit user:

```bash
sudo adduser --system conduit --no-create-home
```

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
Group=nogroup
Restart=always
ExecStart=/usr/local/bin/matrix-conduit

[Install]
WantedBy=multi-user.target
```

Finally, run

```bash
sudo systemctl daemon-reload
```

## Creating the Conduit configuration file

Now we need to create the Conduit's config file in `/etc/matrix-conduit/conduit.toml`. Paste this in **and take a moment
to read it. You need to change at least the server name.**

```toml
{{#include ../../conduit-example.toml}}
```

## Setting the correct file permissions

As we are using a Conduit specific user, we need to allow it to read the config. To do that, you can run this command on
Debian:

```bash
sudo chown -R root:root /etc/matrix-conduit
sudo chmod 755 /etc/matrix-conduit
```

If you use the default database path, you also need to run this:

```bash
sudo mkdir -p /var/lib/matrix-conduit/
sudo chown -R conduit:nogroup /var/lib/matrix-conduit/
sudo chmod 700 /var/lib/matrix-conduit/
```

## Setting up the Reverse Proxy

This depends on whether you use Apache, Nginx or another web server.

### Apache

Create `/etc/apache2/sites-enabled/050-conduit.conf` and copy-and-paste this:

```apache
Listen 8448

<VirtualHost *:443 *:8448>

ServerName your.server.name # EDIT THIS

AllowEncodedSlashes NoDecode
ProxyPass /_matrix/ http://127.0.0.1:6167/_matrix/ nocanon
ProxyPassReverse /_matrix/ http://127.0.0.1:6167/_matrix/

</VirtualHost>
```

**You need to make some edits again.** When you are done, run

```bash
sudo systemctl reload apache2
```

### Nginx

If you use Nginx and not Apache, add the following server section inside the `http` section of `/etc/nginx/nginx.conf`

```nginx
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
sudo systemctl reload nginx
```

## SSL Certificate

The easiest way to get an SSL certificate, if you don't have one already, is to install `certbot` and run this:

```bash
sudo certbot -d your.server.name
```

## You're done!

Now you can start Conduit with:

```bash
sudo systemctl start conduit
```

Set it to start automatically when your system boots with:

```bash
sudo systemctl enable conduit
```

## How do I know it works?

You can open <https://app.element.io>, enter your homeserver and try to register.

You can also use these commands as a quick health check.

```bash
curl https://your.server.name/_matrix/client/versions
curl https://your.server.name:8448/_matrix/client/versions
```

- To check if your server can talk with other homeservers, you can use the [Matrix Federation Tester](https://federationtester.matrix.org/)
- If you want to set up an Appservice, take a look at the [Appservice Guide](../appservices.md).
