# Rate Limiting
Conduit [rate-limits](https://en.wikipedia.org/wiki/Rate_limiting) security/privacy sensitive and
resource intensive endpoints, to protect against things like:
- Denial of service attacks, caused by things like overloading the media store
- Abuse by spammers, attempting to make use of your server to send spam in mass, which usually
  leads to your server being put in policy lists, making it unable to participate in many rooms.
> **Note**: The easiest way to prevent this is to disable public registration, and use a strong
  registration token to allow selective registration.
- Brute-force attacks to guess user's password or the servers registration token, the former leading
  to potential impersination, as well as denial of service if an admin account is accessed.

## Presets

By default, Conduit uses the rate-limiting preset `private_small`, but there are more available if
this isn't the type of server you're planning on running:
- `private_small`: The default preset, designed for small private servers (i.e. single-user or for
  family and friends).
- `private_medium`: Designed for medium-sized private servers (e.g. for an entire school class or year-group)
- `public_medium`: For medium-sized public servers (i.e. you intend 20-100 users to actively use it).
- `public_large`: For larger public server (i.e. you intend 200-1000 users to actively use it).

Here is an example configuration using the `private_medium` preset:
```toml
[global.rate_limiting]
preset = "private_medium"
```

## Overrides

Despite the variety of presets available, you may find the presets to be too restrictive and/or liberal.
You can override all the preset configurations directly in the configuration, and if you think your overrides
should be part of the preset, you can contribute and change them!

The overrides are split into `client` and `federation` sections, for limits that apply to the
[client](https://spec.matrix.org/v1.17/client-server-api/) and
[federation](https://spec.matrix.org/v1.17/server-server-api/) APIs respectively, which are both
then split into `target` and `global` sections, which apply to singular [targets](#targets) or to all of them respectively.

### Targets

A target is any client that call's Conduit's API endpoints, and are identified by one of the following:
- A user ID
- A Server Name (domain)
- An appservice ID
- An IP address, if it cannot be addressed by any of the above (i.e. the client is not authenticated)

The rate limiting configurations under both `target` parts allow you to configure how many
resources/requests each unique client can access within the configured timeframe.
For example, while on a small server you might allow for all logged-in users to send out 100 invites
per day between them, you can set a cap of 5 for each individual user, not only so that they can't
use up the entire global cap, but also prevent potential spam from being spread by that user alone.

### Restrictions

Restrictions are one-to-many mappings to endpoints that have potential for abuse. Like the overrides mentioned above,
they are split into `client` and `federation` restrictions.

#### Client

{{#include ../../target/docs/rate-limiting.md:client-restrictions}}

#### Federation

{{#include ../../target/docs/rate-limiting.md:federation-restrictions}}
