# Prebaked build image for Clawde Linux release legs.
#
# Base must match LINUX_BUILD_IMAGE in scripts/build.sh (glibc 2.36 floor).
# Bakes the apt layer that container_build_leg() would otherwise reinstall
# on every leg (the biggest per-leg waste), plus the aarch64 rustup target.
# The marker file lets the build script detect the prebaked layer and skip
# apt entirely.
#
# Rebuild when the apt lists in scripts/build.sh change or the base image
# bumps a major way (e.g. bookworm -> trixie):
#
#   docker build -t clawde-build:latest -f scripts/docker/clawde-build.Dockerfile scripts/docker/

FROM rust:1.98-bookworm

RUN dpkg --add-architecture arm64 \
    && apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends -o Acquire::Retries=5 \
        pkg-config \
        libasound2-dev \
        cmake \
        golang-go \
        ninja-build \
        libclang-dev \
        gcc-aarch64-linux-gnu \
        g++-aarch64-linux-gnu \
        libc6-dev:arm64 \
        linux-libc-dev:arm64 \
        libasound2-dev:arm64 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add aarch64-unknown-linux-gnu \
    && touch /opt/clawde-prebaked
