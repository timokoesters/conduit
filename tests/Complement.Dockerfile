FROM valkum/docker-rust-ci:latest as builder
WORKDIR /build

ARG RUSTC_WRAPPER
ARG AWS_ACCESS_KEY_ID
ARG AWS_SECRET_ACCESS_KEY
ARG SCCACHE_BUCKET
ARG SCCACHE_ENDPOINT
ARG SCCACHE_S3_USE_SSL

COPY . .
RUN cargo build

FROM valkum/docker-rust-ci:latest
WORKDIR /build

RUN curl -OL "https://github.com/caddyserver/caddy/releases/download/v2.1.1/caddy_2.1.1_linux_amd64.tar.gz"
RUN tar xzf caddy_2.1.1_linux_amd64.tar.gz

COPY --from=builder /build/target/debug/conduit /conduit

ENV SERVER_NAME=localhost
COPY Rocket-example.toml Rocket.toml
RUN sed -i "s/server_name: your.server.name/server_name: ${SERVER_NAME}/g" Rocket.toml
RUN sed -i "s/port = 14004/port: 8008/g" Rocket.toml

EXPOSE 8008 8448
CMD caddy --from 8448 --to localhost:8008 & && /conduit