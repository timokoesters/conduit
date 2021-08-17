# Deploy using Docker

> **Note:** To run and use Conduit you should probably use it with a Domain or Subdomain behind a reverse proxy (like Nginx, Traefik, Apache, ...) with a Lets Encrypt certificate.


## Docker

### Build & Dockerfile

The Dockerfile provided by Conduit has two stages, each of which creates an image.
1. **Builder:** Builds the binary from local context or by cloning a git revision from the official repository.
2. **Runtime:** Copies the built binary from **Builder** and sets up the runtime environment, like creating a volume to persist the database and applying the correct permissions.

The Dockerfile includes a few build arguments that should be supplied when building it.

``` Dockerfile
ARG LOCAL=false
ARG CREATED
ARG VERSION
ARG GIT_REF=origin/master
```

- **CREATED:** Date and time as string (date-time as defined by RFC 3339). Will be used to create the Open Container Initiative compliant label `org.opencontainers.image.created`. Supply by it like this `$(date -u +'%Y-%m-%dT%H:%M:%SZ')`
- **VERSION:** The SemVer version of Conduit, which is in the image. Will be used to create the Open Container Initiative compliant label `org.opencontainers.image.version`. If you have a `Cargo.toml` in your build context, you can get it with `$(grep -m1 -o '[0-9].[0-9].[0-9]' Cargo.toml)`
- **LOCAL:** *(Optional)* A boolean value, specifies if the local build context should be used, or if the official repository will be cloned. If not supplied with the build command, it will default to `false`.
- **GIT_REF:** *(Optional)* A git ref, like `HEAD` or a commit ID. The supplied ref will be used to create the Open Container Initiative compliant label `org.opencontainers.image.revision` and will be the ref that is cloned from the repository when not building from the local context. If not supplied with the build command, it will default to `origin/master`.

To build the image you can use the following command

``` bash
docker build . -t matrixconduit/matrix-conduit:latest --build-arg CREATED=$(date -u +'%Y-%m-%dT%H:%M:%SZ') --build-arg VERSION=$(grep -m1 -o '[0-9].[0-9].[0-9]' Cargo.toml)
```

which also will tag the resulting image as `matrixconduit/matrix-conduit:latest`.
**Note:** it ommits the two optional `build-arg`s.


### Run

After building the image you can simply run it with

``` bash
docker run -d -p 8448:6167 -v ~/conduit.toml:/srv/conduit/conduit.toml -v db:/srv/conduit/.local/share/conduit matrixconduit/matrix-conduit:latest
```

or you can skip the build step and pull the image from one of the following registries:

| Registry        | Image                                                                                                               | Size                                                                                        |
| --------------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| Docker Hub      | [matrixconduit/matrix-conduit:latest](https://hub.docker.com/r/matrixconduit/matrix-conduit)                        | ![Image Size](https://img.shields.io/docker/image-size/matrixconduit/matrix-conduit/latest) |
| GitLab Registry | [registry.gitlab.com/famedly/conduit/conduit:latest](https://gitlab.com/famedly/conduit/container_registry/2134341) | ![Image Size](https://img.shields.io/docker/image-size/matrixconduit/matrix-conduit/latest) |

The `-d` flag lets the container run in detached mode. You now need to supply a `conduit.toml` config file, an example can be found [here](../conduit-example.toml).
You can pass in different env vars to change config values on the fly. You can even configure Conduit completely by using env vars, but for that you need
to pass `-e CONDUIT_CONFIG=""` into your container. For an overview of possible values, please take a look at the `docker-compose.yml` file.

If you just want to test Conduit for a short time, you can use the `--rm` flag, which will clean up everything related to your container after you stop it.


## Docker-compose

If the docker command is not for you or your setup, you can also use one of the provided `docker-compose` files. Depending on your proxy setup, use the [`docker-compose.traefik.yml`](docker-compose.traefik.yml) and [`docker-compose.override.traefik.yml`](docker-compose.override.traefik.yml) for Traefik (don't forget to remove `.traefik` from the filenames) or the normal [`docker-compose.yml`](../docker-compose.yml) for every other reverse proxy. Additional info about deploying
Conduit can be found [here](../DEPLOY.md).


### Build

To build the Conduit image with docker-compose, you first need to open and modify the `docker-compose.yml` file. There you need to comment the `image:` option and uncomment the `build:` option. Then call docker-compose with:

``` bash
CREATED=$(date -u +'%Y-%m-%dT%H:%M:%SZ') VERSION=$(grep -m1 -o '[0-9].[0-9].[0-9]' Cargo.toml) docker-compose up
```

This will also start the container right afterwards, so if want it to run in detached mode, you also should use the `-d` flag. For possible `build-args`, please take a look at the above `Build & Dockerfile` section.


### Run

If you already have built the image or want to use one from the registries, you can just start the container and everything else in the compose file in detached mode with:

``` bash
docker-compose up -d
```

> **Note:** Don't forget to modify and adjust the compose file to your needs.

### Use Traefik as Proxy

As a container user, you probably know about Traefik. It is a easy to use reverse proxy for making containerized app and services available through the web. With the
two provided files, [`docker-compose.traefik.yml`](docker-compose.traefik.yml) and [`docker-compose.override.traefik.yml`](docker-compose.override.traefik.yml), it is
equally easy to deploy and use Conduit, with a little caveat. If you already took a look at the files, then you should have seen the `well-known` service, and that is
the little caveat. Traefik is simply a proxy and loadbalancer and is not able to serve any kind of content, but for Conduit to federate, we need to either expose ports
`443` and `8448` or serve two endpoints `.well-known/matrix/client` and `.well-known/matrix/server`.

With the service `well-known` we use a single `nginx` container that will serve those two files.

So...step by step:

1. Copy [`docker-compose.traefik.yml`](docker-compose.traefik.yml) and [`docker-compose.override.traefik.yml`](docker-compose.override.traefik.yml) from the repository and remove `.traefik` from the filenames.
2. Open both files and modify/adjust them to your needs. Meaning, change the `CONDUIT_SERVER_NAME` and the volume host mappings according to your needs.
3. Create the `conduit.toml` config file, an example can be found [here](../conduit-example.toml), or set `CONDUIT_CONFIG=""` and configure Conduit per env vars.
4. Uncomment the `element-web` service if you want to host your own Element Web Client and create a `element_config.json`.
5. Create the files needed by the `well-known` service.
    - `./nginx/matrix.conf` (relative to the compose file, you can change this, but then also need to change the volume mapping)
      ```nginx
      server {
          server_name <SUBDOMAIN>.<DOMAIN>;
          listen      80 default_server;

          location /.well-known/matrix/ {
              root /var/www;
              default_type application/json;
              add_header Access-Control-Allow-Origin *;
          }
      }
      ```
    - `./nginx/www/.well-known/matrix/client` (relative to the compose file, you can change this, but then also need to change the volume mapping)
      ```json
      {
          "m.homeserver": {
              "base_url": "https://<SUBDOMAIN>.<DOMAIN>"
          }
      }
      ```
    - `./nginx/www/.well-known/matrix/server` (relative to the compose file, you can change this, but then also need to change the volume mapping)
      ```json
      {
          "m.server": "<SUBDOMAIN>.<DOMAIN>:443"
      }
      ```
6. Run `docker-compose up -d`
7. Connect to your homeserver with your preferred client and create a user. You should do this immediatly after starting Conduit, because the first created user is the admin.
