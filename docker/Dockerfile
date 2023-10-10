# Use the rust image as the base image for the build stage
FROM rust:latest AS builder

# Receive architecture as an argument
ARG TARGET_ARCH

# Copy the source code
COPY . /tmp/sim-ln

# Install the dependencies required for building sim-ln
RUN apt-get update \
    && apt-get -y --no-install-recommends install protobuf-compiler musl-tools

RUN cd /tmp/sim-ln \
    && rustup target add ${TARGET_ARCH} \
    && rustup component add rustfmt \
    && RUSTFLAGS='-C target-feature=+crt-static' cargo build --locked --release --target=${TARGET_ARCH}

# Use a new stage with a smaller base image to reduce image size
FROM alpine:latest

ARG TARGET_ARCH

RUN apk update && apk upgrade

# Copy the sim-cli binaries from the build stage to the new stage
COPY --from=builder /tmp/sim-ln/target/${TARGET_ARCH}/release/sim-cli /usr/local/bin/

# Copy the entrypoint script to the container
COPY docker/entrypoint.sh /entrypoint.sh

RUN chmod +x entrypoint.sh

ENTRYPOINT [ "/entrypoint.sh" ]
