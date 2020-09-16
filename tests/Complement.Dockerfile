FROM valkum/docker-rust-ci:latest
WORKDIR /build

COPY . .
RUN cargo build

ENV SERVER_NAME=localhost
EXPOSE 14004 8448

CMD sed "s/server_name: your.server.name/server_name: ${SERVER_NAME}/g" Rocket-example.toml Rocket.toml && ./target/debug/conduit