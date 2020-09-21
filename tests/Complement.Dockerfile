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

FROM nginx:latest
WORKDIR /workdir


COPY --from=builder /workdir/target/debug/conduit /workdir/conduit
COPY tests/start.sh start.sh

COPY Rocket-example.toml Rocket.toml

ENV SERVER_NAME=localhost

RUN sed -i "s/server_name = \"your.server.name\"/server_name = \"${SERVER_NAME}\"/g" Rocket.toml
RUN sed -i "s/port = 14004/port = 8008/g" Rocket.toml

EXPOSE 8008 8448
CMD /workdir/start.sh