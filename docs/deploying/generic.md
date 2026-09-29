# Generic deployment documentation

> ## Getting help
>
> If you run into any problems while setting up Conduit, write an email to `conduit@koesters.xyz`, ask us
> in `#conduit:ahimsa.chat` or [open an issue on GitLab](https://gitlab.com/famedly/conduit/-/issues/new).

## Installing Conduit

Although you might be able to compile Conduit for Windows, we do recommend running it on a Linux server. We therefore
only offer Linux binaries.

You may simply download the binary that fits your machine. Run `uname -m` to see what you need. For `arm`, you should use `aarch`. Now copy the appropriate url:

**Stable/Main versions:**

| Target | Type | Download |
|-|-|-|
| `x86_64-unknown-linux-musl` | Statically linked Debian package | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/master/raw/x86_64-unknown-linux-musl.deb?job=artifacts) |
| `aarch64-unknown-linux-musl` | Statically linked Debian package | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/master/raw/aarch64-unknown-linux-musl.deb?job=artifacts) |
| `x86_64-unknown-linux-musl` | Statically linked binary | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/master/raw/x86_64-unknown-linux-musl?job=artifacts) |
| `aarch64-unknown-linux-musl` | Statically linked binary | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/master/raw/aarch64-unknown-linux-musl?job=artifacts) |
| `x86_64-unknown-linux-gnu` | OCI image | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/master/raw/oci-image-amd64.tar.gz?job=artifacts) |
| `aarch64-unknown-linux-musl` | OCI image | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/master/raw/oci-image-arm64v8.tar.gz?job=artifacts) |

These builds were created on and linked against the glibc version shipped with Debian bullseye.
If you use a system with an older glibc version (e.g. RHEL8), you might need to compile Conduit yourself.

**Latest/Next versions:**

| Target | Type | Download |
|-|-|-|
| `x86_64-unknown-linux-musl` | Statically linked Debian package | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/x86_64-unknown-linux-musl.deb?job=artifacts) |
| `aarch64-unknown-linux-musl` | Statically linked Debian package | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/aarch64-unknown-linux-musl.deb?job=artifacts) |
| `x86_64-unknown-linux-musl` | Statically linked binary | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/x86_64-unknown-linux-musl?job=artifacts) |
| `aarch64-unknown-linux-musl` | Statically linked binary | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/aarch64-unknown-linux-musl?job=artifacts) |
| `x86_64-unknown-linux-gnu` | OCI image | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/oci-image-amd64.tar.gz?job=artifacts) |
| `aarch64-unknown-linux-musl` | OCI image | [link](https://gitlab.com/api/v4/projects/famedly%2Fconduit/jobs/artifacts/next/raw/oci-image-arm64v8.tar.gz?job=artifacts) |

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

If you for whatever reason would rather not, or cannot, open port 8448 to the internet, you can
use [delegation](../delegation.md) to route federation traffic over port 443 (or any other port)
instead.

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

Now we need to create the Conduit's config file in
`/etc/matrix-conduit/conduit.toml`. Paste in the contents of
[`conduit-example.toml`](../configuration.md) **and take a moment to read it.
You need to change at least the server name.**
You can also choose to use a different database backend, but right now only `rocksdb` and `sqlite` are recommended.

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
While we don't give advice for any reverse-proxy in specific, the generic advice is as follows:
- Traffic to `/_matrix/*` and `/.well-known/matrix/*` on your `server_name` should be sent to
  Conduit (unless you're using [delegation](../delegation.md) and set a different domain to serve
  Conduit)
- HTTPS is mandatory for Matrix, so ensure you're serving Conduit over HTTPS with valid TLS
  certificates. If you don't know how to do this, we recommend you use
  [Caddy](https://caddyserver.com) as your reverse-proxy, as it handles this for you automatically,
  as well as needing minimal configuration in general.

Make sure to refer to the documentation of your chosen reverse-proxy to make sure it's configured
properly.

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

You can open [a Matrix client](https://matrix.org/ecosystem/clients), enter your homeserver and try to register. If you are using a registration token, use [Element web](https://app.element.io/), [Nheko](https://matrix.org/ecosystem/clients/nheko/) or [SchildiChat web](https://app.schildi.chat/), as they support this feature.

You can also use these commands as a quick health check.

```bash
$ curl https://your.server.name/_matrix/client/versions
```

- To check if your server can talk with other homeservers, you can use the [Matrix Federation Tester](https://federationtester.matrix.org/).
  If you can register but cannot join federated rooms check your config again and also check if the port 8448 is open and forwarded correctly.

# What's next?

## Audio/Video calls

For Audio/Video call functionality see the [TURN Guide](../turn.md).

## Appservices

If you want to set up an appservice, take a look at the [Appservice Guide](../appservices.md).
