# Configuring Conduit

Conduit can be configured via a config file (conventionally called Conduit.toml) or environment variables. If a config
file exists and environment variables are set, environment variables overwrite config options.

You absolutely need to set the environment variable `CONDUIT_CONFIG_FILE` to either point to a config file (
e.g. `CONDUIT_CONFIG_FILE=/etc/conduit/Conduit.toml`) or to an empty string (`CONDUIT_CONFIG_FILE=''`) if you want to
configure Conduit with just environment variables.

## Mandatory config options

Mandatory variables must be configured in order for Conduit to run properly.

### Server Name

- Config file key: `server_name`
- Envirnoment variable: `CONDUIT_SERVER_NAME`
- Default value: _None, you will need to choose your own._

The server_name is the name of this server.  It is used as a suffix for user and room ids.
Example: If you set it to `conduit.rs`, your usernames will look like `@somebody:conduit.rs`.

The Conduit server needs to be reachable at https://your.server.name/ on port 443 (client-server) and 8448 (server-server) OR you can create /.well-known files to redirect requests.
See the [Client-Server specs](https://matrix.org/docs/spec/client_server/latest#get-well-known-matrix-client) and the [Server-Server specs](https://matrix.org/docs/spec/server_server/r0.1.4#get-well-known-matrix-server) for more information.


### Database Path

- Config file key: `database_path`
- Envirnoment variable: `CONDUIT_DATABASE_PATH`
- Default value: _None, but many people like to use `/var/lib/conduit/`_.

A directory where Conduit stores its database and media files.
This directory must exist, have enough free space and be readable and writable by the user Conduit is running as.

What does _enough free space_ mean? It heavily on the amount of messages your Conduit server will see and the 
amount and size of media files users on your Conduit server send.
As a rule of thumb, you should have at least 10 GB of free space left.
You should be comfortable for quite some time with 50 GB.


### TCP Port

- Config file key: `port`
- Environment variable: `CONDUIT_PORT`
- Default value: _None, but many people like to use `6167`_.

The TCP port Conduit will listen on for connections. The port needs to be free (no other program is listeing on it).

Conduit does currently (2021-09) not offer HTTPS by itself. Only unencrypted HTTP requests will be accepted on this port.
Unless you know what you are doing, this port should not be exposed to the internet.
Instead, use a reverse proxy capable of doing TLS to offer your Conduit server to the internet via HTTPS.
See [TODO] for example configurations.


## Optional configuration options

These config options come with defaults and don't need to be configured for Conduit to run.
That said, you should still check them to make sure that your Conduit server behaves like you want it to do.

### Maximum request size

- Config file key: `max_request_size`
- Environment variable: `CONDUIT_MAX_REQUEST_SIZE`
- Default value: `20_000_000` (~= 20 MB)

The maximum size in bytes for incoming requests to Conduit. You can use underscores to improve readability.

This will effectively limit the size for images, videos and other files users on your Conduit server can send.


### Allow Registration?

- Config file key: `allow_registration`
- Environment variable: `CONDUIT_ALLOW_REGISTRATION`
- Default value: `true`
- Possible values: `true`, `false`

It this is set to `false`, no new users can register accounts on your Conduit server.
Already registered users will not be affected from this setting and can continue to user your server.

The first user to ever register on your Conduit server will be considered the admin account and
is automatically invited into the admin room.


### Allow Encryption?

- Config file key: `allow_encryption`
- Environment variable: `CONDUIT_ALLOW_ENCRYPTION`
- Default value: `true`
- Possible values: `true`, `false`

If this is set to `false`, Conduit disables the ability for users to create encrypted chats.
Existing encrypted chats may continue to work.


### Allow federation?

- Config file key: `allow_federation`
- Environment variable: `CONDUIT_ALLOW_FEDERATION`
- Default value: `false`
- Possible values: `true`, `false`

Federation means that users from different Matrix servers can chat with each other.
E.g. `@mathew:matrix.org` can chat with `@timo:conduit.rs`.

If this option is set to `false`, users on your Conduit server can only talk with other users on your Conduit server.

Federation with other servers needs to happen over HTTPS, so make sure you have set up a reverse proxy.


### Jaeger Tracing

- Config file key: `allow_jaeger`
- Environment variable: `CONDUIT_ALLOW_JAEGER`
- Default value: `false`
- Possible values: `true`, `false`

Enable Jaeger to support monitoring and troubleshooting through Jaeger.

If you don't know what Jaeger is, you can safely leave this set to `false`.


### Trusted servers

- Config file key: `trusted_servers`
- Environment variable: `CONDUIT_TRUSTED_SERVERS`
- Default value: `[]`
- Possible values: JSON-Array of server domains, e.g. `["matrix.org"]` or `["matrix.org", "conduit.rs"]`.

Matrix servers have so-called "server keys", which authenticate messages from their users.
Because your Conduit server might not know the server keys from every server it encounters,
it can ask a _trusted server_ for them.
This speeds things up for rooms with people from a lot of different servers.

You should only set this to include trustworthy servers.
Most people consider a good default to be `["matrix.org"]`.

Only relevant if you have federation enabled.

### Maximum request size

- Config file key: ``
- Environment variable: ``
- Default value: ``
- Possible values: `true`, `false`





| Key in Conduit.toml       | Environment variable              | Default value                                   | Description                                                                                                                                                                                                                         |
|---------------------------|-----------------------------------|-------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `max_concurrent_requests` | `CONDUIT_MAX_CONCURRENT_REQUESTS` | `100`                                           | How many requests Conduit sends to other servers at the same time.                                                                                                                                                                  |
| `log`                     | `CONDUIT_LOG`                     | `info,state_res=warn,rocket=off,_=off,sled=off` | Configures which kind of messages Conduit logs.                                                                                                                                                                                     |
| `workers`                 | `CONDUIT_WORKERS`                 | cpu core count * 2                              | How many worker processes are used.                                                                                                                                                                                                 |
| `address`                 | `CONDUIT_ADDRESS`                 | `127.0.0.1`                                     | Which IP address conduit is listening on. 127.0.0.1 means that Conduit can only be accessed from the same server or through a reverse proxy on that server.                                                                         |
| `db_cache_capacity_mb`    | `CONDUIT_DB_CACHE_CAPACITY_MB`    | `200`                                           | The total amount of memory that the database will use. (this needs clearification: In RAM or on disk and for what exactly?)                                                                                                         |