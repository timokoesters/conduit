# Configuration

**Conduit** is configured using a TOML file. The configuration file is loaded from the path specified by the `CONDUIT_CONFIG` environment variable.

> **Note:** The configuration file is required to run Conduit. If the `CONDUIT_CONFIG` environment variable is not set, Conduit will exit with an error.

> **Note:** If you update the configuration file, you must restart Conduit for the changes to take effect

> **Note:** You can also configure Conduit by using `CONDUIT_{field_name}` environment variables. To set values inside a table, use `CONDUIT_{table_name}_{field_name}`. Example: `CONDUIT_WELL_KNOWN_CLIENT="https://matrix.example.org"`

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
| `media` | `table` | See the [media configuration](#media) | See the [media configuration](#media) |
| `emergency_password` | `string` | Set a password to login as the `conduit` user in case of emergency | N/A |
| `well_known` | `table` | Used for [delegation](delegation.md) | See [delegation](delegation.md) |

### Media
The `media` table is used to configure how media is stored and where. Currently, there is only one available
backend, that being `filesystem`. The backend can be set using the `backend` field. Example:
```toml
[global.media]
backend = "filesystem" # the default backend
```

#### Filesystem backend
The filesystem backend has the following fields:
- `path`: The base directory where all the media files will be stored (defaults to
  `${database_path}/media`)
- `directory_structure`: This is a table, used to configure how files are to be distributed within
  the media directory. It has the following fields:
  - `depth`: The number sub-directories that should be created for files (default: `2`)
  - `length`: How long the name of these sub-directories should be (default: `2`)
  For example, a file may regularly have the name `98ea6e4f216f2fb4b69fff9b3a44842c38686ca685f3f55dc48c5d3fb1107be4`
  (The SHA256 digest of the file's content). If `depth` and `length` were both set to `2`, this file would be stored
  at `${path}/98/ea/6e4f216f2fb4b69fff9b3a44842c38686ca685f3f55dc48c5d3fb1107be4`. If you want to instead have all
  media files in the base directory with no sub-directories, just set `directory_structure` to be empty, as follows:
  ```toml
  [global.media]
  backend = "filesystem"

  [global.media.directory_structure]
  ```

##### Example:
```toml
[global.media]
backend = "filesystem"
path = "/srv/matrix-media"

[global.media.directory_structure]
depth = 4
length = 2
```

#### Retention policies
Over time, the amount of media will keep growing, even if they were only accessed once.
Retention policies allow for media files to automatically be deleted if they meet certain crietia,
allowing disk space to be saved.

This can be configured via the `retention` field of the media config, which is an array with
"scopes" specified
- `scope`: specifies what type of media this policy applies to. If unset, all other scopes which
  you have not configured will use this as a default. Possible values: `"local"`, `"remote"`,
  `"thumbnail"`
- `accessed`: the maximum amount of time since the media was last accessed,
  in the form specified by [`humantime::parse_duration`](https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html)
  (e.g. `"240h"`, `"1400min"`, `"2months"`, etc.)
- `created`: the maximum amount of time since the media was created after, in the same format as
  `accessed` above.
- `space`: the maximum amount of space all of the media in this scope can occupy (if no scope is
  specified, this becomes the total for **all** media). If the creation/downloading of new media,
  will cause this to be exceeded, the last accessed media will be deleted repetitively until there
  is enough space for the new media. The format is specified by [`ByteSize`](https://docs.rs/bytesize/2.0.1/bytesize/index.html)
  (e.g. `"10000MB"`, `"15GiB"`, `"1.5TB"`, etc.)

Media needs to meet **all** the specified requirements to be kept, otherwise, it will be deleted.
This means that thumbnails have to meet both the `"thumbnail"`, and either `"local"` or `"remote"`
requirements in order to be kept.

If the media does not meet the `accessed` or `created` requirement, they will be deleted during a
periodic cleanup, which happens every 1/10th of the period of the shortest retention time, with a
maximum frequency of every minute, and a minimum of every 24 hours. For example, if I set my
`accessed` time for all media to `"2months"`, but override that to be `"48h"` for thumbnails,
the cleanup will happen every 4.8 hours.

##### Example
```toml
# Total of 40GB for all media
[[global.media.retention]] # Notice the double "[]", due to this being a table item in an array
space = "40G"

# Delete remote media not accessed for 30 days, or older than 90 days
[[global.media.retention]]
scope = "remote"
accessed = "30d"
created = "90days" # you can mix and match between the long and short format

# Delete local media not accessed for 1 year
[[global.media.retention]]
scope = "local"
accessed = "1y"

# Only store 1GB of thumbnails
[[global.media.retention]]
scope = "thumbnail"
space = "1GB"

```

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
