# Deploy using Docker

{{#include ../_getting_help.md}}

> **Note:** To run and use Conduit you should probably use it with a Domain or Subdomain behind a reverse proxy (like Nginx, Traefik, Apache, ...) with a Lets Encrypt certificate.

### Get the image

[![Image Size][shield]][dh] [![Image pulls][pulls]][dh]

You can download the latest release version of Conduit as a pre-built multi-arch docker image from [Docker Hub][dh]:

```bash
docker pull matrixconduit/matrix-conduit:latest
```

If you are feeling adventurous, you can also use the unstable, in-development version:

```bash
docker pull matrixconduit/matrix-conduit:next
```

[dh]: https://hub.docker.com/r/matrixconduit/matrix-conduit
[shield]: https://img.shields.io/docker/image-size/matrixconduit/matrix-conduit/latest
[pulls]: https://img.shields.io/docker/pulls/matrixconduit/matrix-conduit

\

<details>
<summary>Want to build your own image?</summary>

Clone the repo and enter it:

```bash
git clone --depth 1 https://gitlab.com/famedly/conduit.git
cd conduit
```

Then, build the image:

```bash
docker build --tag matrixconduit/matrix-conduit:latest .
```

> ⏳ This can take quite some time, as it does a full release build. Depending on your hardware, this may take between 15 and 60 minutes.

If you want to change the userid:groupid under which Conduit will run in the container, you can customize them with build args:

```bash
docker build \
  --build-arg USER_ID=1000 \
  --build-arg GROUP_ID=1000 \
  --tag matrixconduit/matrix-conduit:latest .
```

By default, `USER_ID` and `GROUP_ID` are both set to `1000`.

</details>

### Run

```bash
docker run -d -p 6167:6167 \
  -v db:/var/lib/matrix-conduit/ \
  -e CONDUIT_SERVER_NAME="your.server.name" \
  -e CONDUIT_DATABASE_BACKEND="rocksdb" \
  -e CONDUIT_ALLOW_REGISTRATION="true" \
  -e CONDUIT_ALLOW_FEDERATION="true" \
  -e CONDUIT_MAX_REQUEST_SIZE="20_000_000" \
  -e CONDUIT_TRUSTED_SERVERS='[\"matrix.org\"]' \
  -e CONDUIT_MAX_CONCURRENT_REQUESTS="100" \
  -e CONDUIT_LOG="info,rocket=off,_=off,sled=off" \
  --name "conduit" \
  matrixconduit/matrix-conduit:latest
```

The `-d` flag lets the container run in detached mode.

> ⚠️ When running Conduit with docker, you are expected to configure it only with environment variables, not via a config.toml.
>
> Where you would use `server_name` in the config.toml, use `CONDUIT_SERVER_NAME` as the env var.

If you just want to test Conduit for a short time, you can also supply the `--rm` flag, which will clean up everything related to your container after you stop it.

## Docker-compose

If the `docker run` command is not for you or your setup, you can also use one of the provided `docker-compose` files.

Depending on your proxy setup, you can use one of the following files;

- If you already have a `traefik` instance set up, use [`docker-compose.for-traefik.yml`](docker-compose.for-traefik.yml)
- If you don't have a `traefik` instance set up (or any other reverse proxy), use [`docker-compose.with-traefik.yml`](docker-compose.with-traefik.yml)
- For any other reverse proxy, use [`docker-compose.yml`](docker-compose.yml)

When picking the traefik-related compose file, rename it, so it matches `docker-compose.yml`, and
rename the override file to `docker-compose.override.yml`. Edit the latter with the values you want
for your server.

Additional info about deploying Conduit can be found [here](../DEPLOY.md).

### Build

To build the Conduit image with docker-compose, you first need to open and modify the `docker-compose.yml` file. There you need to comment the `image:` option and uncomment the `build:` option. Then call docker-compose with:

```bash
docker-compose up
```

This will also start the container right afterwards, so if want it to run in detached mode, you also should use the `-d` flag.

### Run

If you already have built the image or want to use one from the registries, you can just start the container and everything else in the compose file in detached mode with:

```bash
docker-compose up -d
```

> **Note:** Don't forget to modify and adjust the compose file to your needs.

### Use Traefik as Proxy

As a container user, you probably know about Traefik. It is an easy to use reverse proxy for making
containerized app and services available through the web. With the two provided files,
[`docker-compose.for-traefik.yml`](docker-compose.for-traefik.yml) (or
[`docker-compose.with-traefik.yml`](docker-compose.with-traefik.yml)) and
[`docker-compose.override.yml`](docker-compose.override.traefik.yml), it is equally easy to deploy
and use Conduit, with a little caveat. If you already took a look at the files, then you should have
seen the `well-known` service, and that is the little caveat. Traefik is simply a proxy and
loadbalancer and is not able to serve any kind of content, but for Conduit to federate, we need to
either expose ports `443` and `8448` or serve two endpoints `.well-known/matrix/client` and
`.well-known/matrix/server`.

With the service `well-known` we use a single `nginx` container that will serve those two files.

So... step by step:

1. Copy [`docker-compose.traefik.yml`](docker-compose.traefik.yml) and [`docker-compose.override.traefik.yml`](docker-compose.override.traefik.yml) from the repository and remove `.traefik` from the filenames.
2. Open both files and modify/adjust them to your needs. Meaning, change the `CONDUIT_SERVER_NAME` and the volume host mappings according to your needs.
3. Configure Conduit per env vars.
4. Uncomment the `element-web` service if you want to host your own Element Web Client and create a `element_config.json`.
5. Create the files needed by the `well-known` service.

   - `./nginx/matrix.conf` (relative to the compose file, you can change this, but then also need to change the volume mapping)

     ```nginx
     server {
         server_name <SUBDOMAIN>.<DOMAIN>;
         listen      80 default_server;

         location /.well-known/matrix/server {
            return 200 '{"m.server": "<SUBDOMAIN>.<DOMAIN>:443"}';
            add_header Content-Type application/json;
         }

        location /.well-known/matrix/client {
            return 200 '{"m.homeserver": {"base_url": "https://<SUBDOMAIN>.<DOMAIN>"}}';
            add_header Content-Type application/json;
            add_header "Access-Control-Allow-Origin" *;
        }

        location / {
            return 404;
        }
     }
     ```

6. Run `docker-compose up -d`
7. Connect to your homeserver with your preferred client and create a user. You should do this immediately after starting Conduit, because the first created user is the admin.
