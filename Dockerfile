# Platforms might be linux/amd64, linux/arm64

FROM --platform=$BUILDPLATFORM docker.io/rust:1.75-bookworm AS builder

RUN apt-get update && \
    apt-get -y --no-install-recommends install libclang-dev

# Install Zig
ARG ZIG_VERSION=0.11.0
RUN curl -L "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-$(uname -m)-${ZIG_VERSION}.tar.xz" | tar -J -x -C /usr/local && \
    ln -s "/usr/local/zig-linux-$(uname -m)-${ZIG_VERSION}/zig" /usr/local/bin/zig

ARG TARGETARCH

# Install zigbuild
RUN cargo install cargo-zigbuild

RUN <<EOL
    case $TARGETARCH in \
          (amd64)   rustarch="x86_64-unknown-linux-gnu";; \
          (arm64)   rustarch="aarch64-unknown-linux-gnu";; \
          (arm)   rustarch="armv7-unknown-linux-gnueabihf";; \
    esac
    echo "$TARGETARCH"
    rustup target add "$rustarch"
EOL

## # == Build dependencies without our own code separately for caching ==
## #
## # Need a fake main.rs since Cargo refuses to build anything otherwise.
## #
## # See https://github.com/rust-lang/cargo/issues/2644 for a Cargo feature
## # request that would allow just dependencies to be compiled, presumably
## # regardless of whether source files are available.
## COPY Cargo.toml Cargo.lock ./
## 
## RUN <<EOL
##     case $TARGETARCH in \
##           (amd64)   rustarch="x86_64-unknown-linux-gnu";; \
##           (arm64)   rustarch="aarch64-unknown-linux-gnu";; \
##           (arm)   rustarch="armv7-unknown-linux-gnueabihf";; \
##     esac
## 
##     mkdir src && touch src/lib.rs && echo 'fn main() {}' > src/main.rs
##     cargo zigbuild --target --release "$rustarch"
##     rm -r src
## EOL

# Copy over actual Conduit sources
COPY Cargo.toml Cargo.lock ./
COPY src src


# main.rs and lib.rs need their timestamp updated for this to work correctly since
# otherwise the build with the fake main.rs from above is newer than the
# source files (COPY preserves timestamps).
#
# Builds conduit and places the binary at /conduit
RUN <<EOL
    case $TARGETARCH in \
          (amd64)   rustarch="x86_64-unknown-linux-gnu";; \
          (arm64)   rustarch="aarch64-unknown-linux-gnu";; \
          (arm)   rustarch="armv7-unknown-linux-gnueabihf";; \
    esac

    touch src/main.rs
    touch src/lib.rs
    cargo zigbuild --release --target "$rustarch"
    mv "target/$rustarch/debug/conduit" /conduit
EOL




# On the target arch:
FROM docker.io/debian:bookworm-slim AS runtime

# Standard port on which Conduit launches.
# You still need to map the port when using the docker command or docker-compose.
EXPOSE 6167

ARG DEFAULT_DB_PATH=/var/lib/matrix-conduit

ENV CONDUIT_PORT=6167 \
    CONDUIT_ADDRESS="0.0.0.0" \
    CONDUIT_DATABASE_PATH=${DEFAULT_DB_PATH} \
    CONDUIT_CONFIG=''
#    └─> Set no config file to do all configuration with env vars

# Test if Conduit is still alive, uses the same endpoint as Element
COPY ./docker/healthcheck.sh /srv/conduit/healthcheck.sh
HEALTHCHECK --start-period=5s --interval=5s CMD ./healthcheck.sh

# Conduit needs:
#   ca-certificates: for https
#   iproute2 & wget: for the healthcheck script
RUN apt-get update && apt-get -y --no-install-recommends install \
    ca-certificates \
    iproute2 \
    wget \
    && rm -rf /var/lib/apt/lists/*


# Improve security: Don't run stuff as root, that does not need to run as root
# Most distros also use 1000:1000 for the first real user, so this should resolve volume mounting problems.
ARG USER_ID=1000
ARG GROUP_ID=1000
RUN set -x ; \
    groupadd -r -g ${GROUP_ID} conduit ; \
    useradd -l -r -M -d /srv/conduit -o -u ${USER_ID} -g conduit conduit && exit 0 ; exit 1

# Create database directory, change ownership of Conduit files to conduit user and group and make the healthcheck executable:
RUN chown -cR conduit:conduit /srv/conduit && \
    chmod +x /srv/conduit/healthcheck.sh && \
    mkdir -p ${DEFAULT_DB_PATH} && \
    chown -cR conduit:conduit ${DEFAULT_DB_PATH}

# Change user to conduit, no root permissions afterwards:
USER conduit
# Set container home directory
WORKDIR /srv/conduit

# Run Conduit and print backtraces on panics
ENV RUST_BACKTRACE=1
ENTRYPOINT [ "/usr/sbin/matrix-conduit" ]



FROM runtime AS final

# Actually copy over conduit binary
COPY --from=builder /conduit /usr/sbin/matrix-conduit
