
FROM matrixconduit/matrix-conduit:next-alpine AS conduit-complement
WORKDIR /workdir
USER root

RUN apk add --no-cache caddy

ENV ROCKET_LOG=normal \
    CONDUIT_LOG="info,rocket=info,_=off,sled=off" \
    CONDUIT_CONFIG="" \
    CONDUIT_DATABASE_PATH="/tmp/" \
    CONDUIT_SERVER_NAME=localhost \
    CONDUIT_ADDRESS="0.0.0.0" \
    CONDUIT_PORT="6167" \
    CONDUIT_ALLOW_FEDERATION="true" \
    CONDUIT_ALLOW_ENCRYPTION="true" \
    CONDUIT_ALLOW_REGISTRATION="true"


# Enabled Caddy auto cert generation for complement provided CA.
COPY ./tests/complement-caddy.json ./caddy.json 

EXPOSE 8008 8448

HEALTHCHECK --start-period=2s --interval=2s CMD true
ENTRYPOINT [""]
CMD ([ -z "${COMPLEMENT_CA}" ] && echo "Error: Need Complement PKI support" && true) || \
    cp /ca/ca.crt /usr/local/share/ca-certificates/complement.crt && update-ca-certificates && \
    export CONDUIT_SERVER_NAME="${SERVER_NAME}" && \
    sed -i "s/your.server.name/${SERVER_NAME}/g" caddy.json && \
    (caddy start --config caddy.json) >> /tmp/caddy.log 2>> /tmp/caddy.err.log && \
    echo "Starting Conduit with address '${SERVER_NAME}'" && \
    /srv/conduit/conduit
