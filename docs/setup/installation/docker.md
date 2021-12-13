# Deploy using Docker

> **Note:** To run and use Conduit you should probably use it with a Domain or Subdomain behind a reverse proxy (like Nginx, Traefik, Apache, ...) with a Lets Encrypt certificate.
>
> See the [Domain section](../domain.md) for more about this.

## Standalone Docker image

A typical way to start Conduit with Docker looks like this:

```bash
docker run \
  --name "conduit" \
  --detach \
  --restart "unless-stopped" \
  --env CONDUIT_CONFIG="" \
  --env CONDUIT_SERVER_NAME="domain.tld" \
  --env CONDUIT_ADDRESS="0.0.0.0" \
  --env CONDUIT_ALLOW_REGISTRATION="true" \
  --env CONDUIT_ALLOW_FEDERATION="true" \
  --env CONDUIT_DATABASE_PATH="/srv/conduit/.local/share/conduit" \
  --volume "/var/lib/conduit/:/srv/conduit/.local/share/conduit" \
  --publish 6167:6167
  matrixconduit/matrix-conduit:latest
```

<details>
<summary>Explanation of the above command</summary>

- `--name "conduit"` Create a container named "conduit"
- `--detach` Detach from current terminal and run in the background
- `--restart=unless-stopped` Restart if Conduit crashes or after reboots
- `--env CONDUIT_CONFIG=""` Tell Conduit to only use environment variables (instead of a config file)
- `--env CONDUIT_ADDRESS="0.0.0.0" ` Answer to requests from outside of the container...
- `--publish 6167:6167` ... on port 6167

</details>

After a few seconds, your Conduit should be listening on port 6167.
If you have Element Desktop installed on the same machine, try creating an account on the server `localhost:6167`.

To check how your Conduit container is doing, you can use the commands `docker ps` and `docker logs conduit`.

### Next steps

For a functioning Matrix server which you can connect to from your phone and which federates with other Matrix servers, you still need to configure a reverse proxy to:

- Forward https traffic as http to the Conduit container on port 6167
- Serve .well-known files (see the [Domain section](../domain.md)) to tell Servers and clients where to find your Conduit
- Optionally serve a Matrix Web Client like Element Web or FluffyChat Web.

## Docker Compose

We also provide a `docker-compose.yaml` file, which includes everything you need to run a complete Matrix Homeserver:

- Conduit
- The reverse proxy
- Matrix Web Client

To get started:

1. Copy the `docker-compose.yaml` file to a new directory on your server.

2. Edit it and adjust your configuration.

3. Start it with

```bash
docker-compose up .d
```

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
