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

RUN curl -OL "https://github.com/caddyserver/caddy/releases/download/v2.1.1/caddy_2.1.1_linux_amd64.tar.gz"
RUN tar xzf caddy_2.1.1_linux_amd64.tar.gz

COPY --from=builder /workdir/target/debug/conduit /workdir/conduit

COPY Rocket-example.toml Rocket.toml

ENV SERVER_NAME=localhost

RUN sed -i "s/server_name = \"your.server.name\"/server_name = \"${SERVER_NAME}\"/g" Rocket.toml
RUN sed -i "s/port = 14004/port = 8008/g" Rocket.toml

EXPOSE 8008 8448
CMD /workdir/caddy reverse-proxy --from ${SERVER_NAME}:8448 --to localhost:8008 > /dev/null 2>&1 & /workdir/conduit