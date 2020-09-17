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
COPY --from=builder /build/target/debug/conduit /conduit

ENV SERVER_NAME=localhost
EXPOSE 14004 8448

CMD sed "s/server_name: your.server.name/server_name: ${SERVER_NAME}/g" Rocket-example.toml Rocket.toml && /conduit