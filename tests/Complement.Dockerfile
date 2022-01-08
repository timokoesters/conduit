FROM matrixconduit/matrix-conduit:next-alpine AS conduit-complement

USER root

# TODO: REMOVE
# TODO: REMOVE
# TODO: REMOVE
# TODO: REMOVE
COPY --chown=1000:1000 ./conduit-debug-x86_64-unknown-linux-musl /srv/conduit/conduit
RUN chmod +x /srv/conduit/conduit
# TODO: REMOVE
# TODO: REMOVE
# TODO: REMOVE
# TODO: REMOVE

RUN apk add --no-cache caddy openssl && \
    openssl genrsa -out "/conduit-https.key" 2048

ENV ROCKET_LOG=normal \
    CONDUIT_LOG="info,rocket=info,_=off,sled=off" \
    CONDUIT_CONFIG="" \
    CONDUIT_DATABASE_PATH="/tmp/" \
    CONDUIT_DATABASE_BACKEND="rocksdb" \
    CONDUIT_SERVER_NAME=localhost \
    CONDUIT_ADDRESS="0.0.0.0" \
    CONDUIT_PORT="6167" \
    CONDUIT_ALLOW_FEDERATION="true" \
    CONDUIT_ALLOW_ENCRYPTION="true" \
    CONDUIT_ALLOW_REGISTRATION="true"


COPY ./tests/complement-start.sh ./tests/complement-caddy.json /
RUN chmod +x /complement-start.sh

ENTRYPOINT ["/complement-start.sh"]

EXPOSE 8008 8448

