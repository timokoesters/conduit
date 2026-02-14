FROM rust:1.88.0

WORKDIR /workdir

RUN apt-get update && apt-get install -y --no-install-recommends \
    libclang-dev

COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY src src
COPY .cargo .cargo
RUN cargo build --release \
    && mv target/release/conduit conduit \
    && rm -rf target

# Install caddy
RUN apt-get update \
    && apt-get install -y \
    debian-keyring \
    debian-archive-keyring \
    apt-transport-https \
    curl \
    && curl -1sLf 'https://dl.cloudsmith.io/public/caddy/testing/gpg.key' \
    | gpg --dearmor -o /usr/share/keyrings/caddy-testing-archive-keyring.gpg \
    && curl -1sLf 'https://dl.cloudsmith.io/public/caddy/testing/debian.deb.txt' \
    | tee /etc/apt/sources.list.d/caddy-testing.list \
    && apt-get update \
    && apt-get install -y caddy

COPY conduit-example.toml conduit.toml
COPY complement/Caddyfile Caddyfile

ENV SERVER_NAME=localhost
ENV CONDUIT_CONFIG=/workdir/conduit.toml

RUN sed -i "s/port = 6167/port = 8008/g" conduit.toml
RUN echo "log = \"warn,_=off,sled=off\"" >> conduit.toml
RUN sed -i "s/address = \"127.0.0.1\"/address = \"0.0.0.0\"/g" conduit.toml
RUN sed -i "s/registration_token = \"\"//g" conduit.toml
RUN sed -i "s/allow_check_for_updates = true/allow_check_for_updates = false/g" conduit.toml

EXPOSE 8008 8448

CMD uname -a && \
    sed -i "s/#server_name = \"your.server.name\"/server_name = \"${SERVER_NAME}\"/g" conduit.toml && \
    sed -i "s/your.server.name/${SERVER_NAME}/g" Caddyfile && \
    caddy start > /dev/null && \
    /workdir/conduit
