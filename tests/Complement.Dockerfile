FROM valkum/docker-rust-ci:latest as builder
WORKDIR /workdir

ARG RUSTC_WRAPPER
ARG AWS_ACCESS_KEY_ID
ARG AWS_SECRET_ACCESS_KEY
ARG SCCACHE_BUCKET
ARG SCCACHE_ENDPOINT
ARG SCCACHE_S3_USE_SSL

COPY . .
RUN cargo build

FROM valkum/docker-rust-ci:latest
WORKDIR /workdir

RUN curl -OL "https://github.com/caddyserver/caddy/releases/download/v2.2.1/caddy_2.2.1_linux_amd64.tar.gz"
RUN tar xzf caddy_2.2.1_linux_amd64.tar.gz

COPY --from=builder /workdir/target/debug/conduit /workdir/conduit

COPY Rocket-example.toml Rocket.toml

ENV SERVER_NAME=localhost
ENV ROCKET_LOG=normal

RUN sed -i "s/port = 14004/port = 8008/g" Rocket.toml
RUN echo "federation_enabled = true" >> Rocket.toml

# Enabled Caddy auto cert generation for complement provided CA.
RUN echo '{"apps":{"http":{"https_port":8448,"servers":{"srv0":{"listen":[":8448"],"routes":[{"match":[{"host":["your.server.name"]}],"handle":[{"handler":"subroute","routes":[{"handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:8008"}]}]}]}],"terminal":true}],"tls_connection_policies": [{"match": {"sni": ["your.server.name"]}}]}}},"pki": {"certificate_authorities": {"local": {"name": "Complement CA","root": {"certificate": "/ca/ca.crt","private_key": "/ca/ca.key"},"intermediate": {"certificate": "/ca/ca.crt","private_key": "/ca/ca.key"}}}},"tls":{"automation":{"policies":[{"subjects":["your.server.name"],"issuer":{"module":"internal"},"on_demand":true},{"issuer":{"module":"internal", "ca": "local"}}]}}}}' > caddy.json 
 
EXPOSE 8008 8448

CMD ([ -z "${COMPLEMENT_CA}" ] && echo "Error: Need Complement CA support" && true) || \
    sed -i "s/server_name = \"your.server.name\"/server_name = \"${SERVER_NAME}\"/g" Rocket.toml && \
    sed -i "s/your.server.name/${SERVER_NAME}/g" caddy.json && \
    /workdir/caddy start --config caddy.json > /dev/null && \
    /workdir/conduit
    