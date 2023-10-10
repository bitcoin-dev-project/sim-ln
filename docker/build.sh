#!/bin/bash

case $(uname -m) in
    x86_64)
        ARCH="amd64"
        TARGET_ARCH="x86_64-unknown-linux-musl"
        ;;
    arm64)
        ARCH="arm64"
        TARGET_ARCH="aarch64-unknown-linux-musl"
        ;;
    *)
        echo "Unsupported architecture"
        exit 1
        ;;
esac

echo "Building for architecture: $TARGET_ARCH"
docker build -f docker/Dockerfile --build-arg TARGET_ARCH=$TARGET_ARCH -t sim-ln .
