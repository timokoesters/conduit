# Configuring Conduit

Conduit can be configured via a config file (conventionally called Conduit.toml) or environment variables. If a config
file exists and environment variables are set, environment variables overwrite config options.

You absolutely need to set the environment variable `CONDUIT_CONFIG_FILE` to either point to a config file (
e.g. `CONDUIT_CONFIG_FILE=/etc/conduit/Conduit.toml`) or to an empty string (`CONDUIT_CONFIG_FILE=''`) if you want to
configure Conduit with just environment variables.

Mandatory variables must be configured in order for Conduit to run properly.

| Key in Conduit.toml    | Environment variable         | Default value | Mandatory | Description |
| ---------------------- | ---------------------------- | ------------- | --------- | ----------- |
| `server_name`          | `CONDUIT_SERVER_NAME`        |               | yes       | The server_name is the name of this server. It is used as a suffix for user and room ids. Examples: matrix.org, conduit.rs. The Conduit server needs to be reachable at https://your.server.name/ on port 443 (client-server) and 8448 (server-server) OR you can create /.well-known files to redirect requests. See [Client-Server specs](https://matrix.org/docs/spec/client_server/latest#get-well-known-matrix-client) and [Server-Server specs](https://matrix.org/docs/spec/server_server/r0.1.4#get-well-known-matrix-server) for more information. |
| `database_path`        | `CONDUIT_DATABASE_PATH`      | `/var/lib/conduit/` | yes | A directory Conduit where Conduit stores its database and media files. This directory must exist and be writable before Conduit is started. |
| `port`                 | `CONDUIT_PORT`               | `6167`        | yes       | The port Conduit will be running on. You need to set up a reverse proxy in your web server (e.g. apache or nginx), so all requests to /_matrix on port 443 and 8448 will be forwarded to the Conduit instance running on this port. |
| `max_request_size`     | `CONDUIT_MAX_REQUEST_SIZE`   | `20_000_000`  | no        | The maximum size in bytes for uploads (files sent from users on this Conduit server). Uploads will be stored in the database path, so make sure that it has sufficient free space. |
| `allow_registration`   | `CONDUIT_ALLOW_REGISTRATION` | `true`        | no        | Are new users allowed to register accounts on their own? Possible values: `true`, `false`. |
| `allow_encryption`     | `CONDUIT_ALLOW_ENCRYPTION`   | `true`        | no        | Controls whether encrypted rooms can be created or not. Possible values: `true`, `false`. |
| `allow_federation`     | `CONDUIT_ALLOW_FEDERATION`   | `false`       | no        | Federation enables users on your Conduit server to talk to other Matrix users on different Matrix servers. If federation is turned off, only users on your Conduit server can talk to each other. Possible values: `true`, `false`. |
| `allow_jaeger`         | `CONDUIT_ALLOW_JAEGER`       | `false`       | no        | Enable jaeger to support monitoring and troubleshooting through jaeger. Possible values: `true`, `false`. |
| `trusted_servers`      | `CONDUIT_TRUSTED_SERVERS`    | `[]`          | no        | List of servers, which Conduit trusts enough to ask them for public keys of other, newly found servers. E.g. to trust the matrix.org server, set this value to `["matrix.org"]`. |
| `max_concurrent_requests` | `CONDUIT_MAX_CONCURRENT_REQUESTS` | `100` | no        | How many requests Conduit sends to other servers at the same time. |
| `log`                  | `CONDUIT_LOG`                | `info,state_res=warn,rocket=off,_=off,sled=off` | no | Configures which kind of messages Conduit logs. |
| `workers`              | `CONDUIT_WORKERS`            | cpu core count * 2 | no   | How many worker processes are used.
| `address`              | `CONDUIT_ADDRESS`            | `127.0.0.1`   | no        | Which IP address conduit is listening on. 127.0.0.1 means that Conduit can only be accessed from the same server or through a reverse proxy on that server.
| `db_cache_capacity_mb` | `CONDUIT_DB_CACHE_CAPACITY_MB` | `200`       | no        | The total amount of memory that the database will use. (this needs clearification: In RAM or on disk and for what exactly?)