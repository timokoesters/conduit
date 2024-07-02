# Configuration

**Conduit** is configured using a TOML file. The configuration file is loaded from the path specified by the `CONDUIT_CONFIG` environment variable.

> **Note:** The configuration file is required to run Conduit. If the `CONDUIT_CONFIG` environment variable is not set, Conduit will exit with an error.

> **Note:** If you update the configuration file, you must restart Conduit for the changes to take effect

> **Note:** You can also configure Conduit by using `CONDUIT_{field_name}` environment variables. To set values inside a table, use `CONDUIT_{table_name}__{field_name}`. Example: `CONDUIT_SERVER_NAME="example.org"`

Conduit's configuration file is divided into the following sections:

- [Global](#global)
    - [TLS](#tls)
    - [Proxy](#proxy)


## Global

The `global` section contains the following fields:

> **Note:** The `*` symbol indicates that the field is required, and the values in **parentheses** are the possible values

| Field | Type | Description | Default |
| --- | --- | --- | --- |
| `address` | `string` | The address to bind to | `"127.0.0.1"` |
| `port` | `integer` | The port to bind to | `8000` |
| `tls` | `table` | See the [TLS configuration](#tls) | N/A |
| `server_name`_*_ | `string` | The server name | N/A |
| `database_backend`_*_ | `string` | The database backend to use (`"rocksdb"` *recommended*, `"sqlite"`) | N/A |
| `database_path`_*_ | `string` | The path to the database file/dir | N/A |
| `db_cache_capacity_mb` | `float` | The cache capacity, in MB | `300.0` |
| `enable_lightning_bolt` | `boolean` | Add `⚡️` emoji to end of user's display name | `true` |
| `allow_check_for_updates` | `boolean` | Allow Conduit to check for updates | `true` |
| `conduit_cache_capacity_modifier` | `float` | The value to multiply the default cache capacity by | `1.0` |
| `rocksdb_max_open_files` | `integer` | The maximum number of open files | `1000` |
| `pdu_cache_capacity` | `integer` | The maximum number of Persisted Data Units (PDUs) to cache | `150000` |
| `cleanup_second_interval` | `integer` | How often conduit should clean up the database, in seconds | `60` |
| `max_request_size` | `integer` | The maximum request size, in bytes | `20971520` (20 MiB) |
| `max_concurrent_requests` | `integer` | The maximum number of concurrent requests | `100` |
| `max_fetch_prev_events` | `integer` | The maximum number of previous events to fetch per request if conduit notices events are missing | `100` |
| `allow_registration` | `boolean` | Opens your homeserver to public registration | `false` |
| `registration_token` | `string` | The token users need to have when registering to your homeserver | N/A |
| `allow_encryption` | `boolean` | Allow users to enable encryption in their rooms | `true` |
| `allow_federation` | `boolean` | Allow federation with other servers | `true` |
| `allow_room_creation` | `boolean` | Allow users to create rooms | `true` |
| `allow_unstable_room_versions` | `boolean` | Allow users to create and join rooms with unstable versions | `true` |
| `default_room_version` | `string` | The default room version (`"6"`-`"10"`)| `"10"` |
| `allow_jaeger` | `boolean` | Allow Jaeger tracing | `false` |
| `tracing_flame` | `boolean` | Enable flame tracing | `false` |
| `proxy` | `table` | See the [Proxy configuration](#proxy) | N/A |
| `jwt_secret` | `string` | The secret used in the JWT to enable JWT login without it a 400 error will be returned | N/A |
| `trusted_servers` | `array` | The list of trusted servers to gather public keys of offline servers | `["matrix.org"]` |
| `log` | `string` | The log verbosity to use | `"warn"` |
| `turn_username` | `string` | The TURN username | `""` |
| `turn_password` | `string` | The TURN password | `""` |
| `turn_uris` | `array` | The TURN URIs | `[]` |
| `turn_secret` | `string` | The TURN secret | `""` |
| `turn_ttl` | `integer` | The TURN TTL in seconds | `86400` |
| `emergency_password` | `string` | Set a password to login as the `conduit` user in case of emergency | N/A |
| `well_known` | `table` | Used for [delegation](delegation.md) | See [delegation](delegation.md) |


### TLS
The `tls` table contains the following fields:
- `certs`: The path to the public PEM certificate
- `key`: The path to the PEM private key

#### Example
```toml
[global.tls]
certs = "/path/to/cert.pem"
key = "/path/to/key.pem"
```


### Proxy
You can choose what requests conduit should proxy (if any). The `proxy` table contains the following fields

#### Global
The global option will proxy all outgoing requests. The `global` table contains the following fields:
- `url`: The URL of the proxy server
##### Example
```toml
[global.proxy.global]
url = "https://example.com"
```

#### By domain
An array of tables that contain the following fields:
- `url`: The URL of the proxy server
- `include`: Domains that should be proxied (assumed to be `["*"]` if unset)
- `exclude`: Domains that should not be proxied (takes precedent over `include`)

Both `include` and `exclude` allow for glob pattern matching.
##### Example
In this example, all requests to domains ending in `.onion` and `matrix.secretly-an-onion-domain.xyz` 
will be proxied via `socks://localhost:9050`, except for domains ending in `.myspecial.onion`. You can add as many `by_domain` tables as you need.
```toml
[[global.proxy.by_domain]]
url = "socks5://localhost:9050"
include = ["*.onion", "matrix.secretly-an-onion-domain.xyz"]
exclude = ["*.clearnet.onion"]
```

### Example

> **Note:** The following example is a minimal configuration file. You should replace the values with your own.

```toml
[global]
{{#include ../conduit-example.toml:22:}}
```
