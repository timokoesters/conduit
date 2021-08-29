#!/bin/sh

# If the port is not specified as env var, take it from the config file
if [ -z ${CONDUIT_PORT} ]; then
  CONDUIT_PORT=$(grep -m1 -o 'port\s=\s[0-9]*' conduit.toml | grep -m1 -o '[0-9]*')
fi

# The actual health check.
# We try to first get a response on HTTP and when that fails on HTTPS and when that fails, we exit with code 1.
# TODO: Change this to a single curl call. Do we have a config value that we can check for that?
curl --fail -s "http://localhost:${CONDUIT_PORT}/_matrix/client/versions" || \
    curl -k --fail -s "https://localhost:${CONDUIT_PORT}/_matrix/client/versions" || \
    exit 1
