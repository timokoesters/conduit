# syntax=docker/dockerfile:1
# ---------------------------------------------------------------------------------------------------------
# This Dockerfile is intended to be built as part of Conduit's CI pipeline.
# It does not build Conduit in Docker, but just copies the matching build artifact from the build jobs.
#
# It is mostly based on the normal Conduit Dockerfile, but adjusted in a few places to maximise caching.
# Credit's for the original Dockerfile: Weasy666.
# ---------------------------------------------------------------------------------------------------------

FROM docker.io/alpine:3.22.1@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1 AS runner


# Standard port on which Conduit launches.
# You still need to map the port when using the docker command or docker-compose.
EXPOSE 6167

# Users are expected to mount a volume to this directory:
ARG DEFAULT_DB_PATH=/var/lib/matrix-conduit

ENV CONDUIT_PORT=6167 \
    CONDUIT_ADDRESS="0.0.0.0" \
    CONDUIT_DATABASE_PATH=${DEFAULT_DB_PATH} \
    CONDUIT_CONFIG=''
#    └─> Set no config file to do all configuration with env vars

# Conduit needs:
#   ca-certificates: for https
#   iproute2: for `ss` for the healthcheck script
RUN apk add --no-cache \
    ca-certificates \
    iproute2

ARG CREATED
ARG VERSION
ARG GIT_REF
# Labels according to https://github.com/opencontainers/image-spec/blob/master/annotations.md
# including a custom label specifying the build command
LABEL org.opencontainers.image.created=${CREATED} \
    org.opencontainers.image.authors="Conduit Contributors" \
    org.opencontainers.image.title="Conduit" \
    org.opencontainers.image.version=${VERSION} \
    org.opencontainers.image.vendor="Conduit Contributors" \
    org.opencontainers.image.description="A Matrix homeserver written in Rust" \
    org.opencontainers.image.url="https://conduit.rs/" \
    org.opencontainers.image.revision=${GIT_REF} \
    org.opencontainers.image.source="https://gitlab.com/famedly/conduit.git" \
    org.opencontainers.image.licenses="Apache-2.0" \
    org.opencontainers.image.documentation="https://gitlab.com/famedly/conduit" \
    org.opencontainers.image.ref.name=""


# Test if Conduit is still alive, uses the same endpoint as Element
COPY ./docker/healthcheck.sh /srv/conduit/healthcheck.sh
HEALTHCHECK --start-period=5s --interval=5s CMD ./healthcheck.sh

# Improve security: Don't run stuff as root, that does not need to run as root:
# Most distros also use 1000:1000 for the first real user, so this should resolve volume mounting problems.
ARG USER_ID=1000
ARG GROUP_ID=1000
RUN set -x ; \
    deluser --remove-home www-data ; \
    addgroup -S -g ${GROUP_ID} conduit 2>/dev/null ; \
    adduser -S -u ${USER_ID} -D -H -h /srv/conduit -G conduit -g conduit conduit 2>/dev/null ; \
    addgroup conduit conduit 2>/dev/null && exit 0 ; exit 1

# Change ownership of Conduit files to conduit user and group
RUN chown -cR conduit:conduit /srv/conduit && \
    chmod +x /srv/conduit/healthcheck.sh && \
    mkdir -p ${DEFAULT_DB_PATH} && \
    chown -cR conduit:conduit ${DEFAULT_DB_PATH}

# Change user to conduit
USER conduit
# Set container home directory
WORKDIR /srv/conduit

# Run Conduit and print backtraces on panics
ENV RUST_BACKTRACE=1
ENTRYPOINT [ "/srv/conduit/conduit" ]

# Depending on the target platform (e.g. "linux/arm/v7", "linux/arm64/v8", or "linux/amd64")
# copy the matching binary into this docker image
ARG TARGETPLATFORM
COPY --chown=conduit:conduit ./$TARGETPLATFORM /srv/conduit/conduit
