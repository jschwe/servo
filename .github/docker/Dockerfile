# Dockerfile to build servo in CI
# We use multi-stage builds to reduce the size of the final image and
# allow parallel building of independant layers

FROM ubuntu:22.04 AS base_fetcher

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get -y install --no-install-recommends \
    ca-certificates unzip curl git jq \
    && apt-get clean


FROM base_fetcher AS base_with_cc
# We need a c-compiler to `cargo install` some of our dependencies
RUN apt-get update && \
    apt-get -y install --no-install-recommends \
    build-essential ccache clang cmake python3 \
    && apt-get clean

FROM base_with_cc AS base
# Install the remaining dependencies in a new stage, so that `cargo install` can run in parallel

# Add apt-repository for `gh`
COPY --chmod=644 githubcli-archive-keyring.gpg /etc/apt/keyrings/githubcli-archive-keyring.gpg
RUN echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list

RUN apt-get update && \
    apt-get -y install --no-install-recommends \
        libdbus-1-dev libfreetype6-dev libgl1-mesa-dri libgles2-mesa-dev \
        libglib2.0-dev gstreamer1.0-plugins-good libgstreamer-plugins-good1.0-dev \
        gstreamer1.0-plugins-bad libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-ugly \
        gstreamer1.0-plugins-base libgstreamer-plugins-base1.0-dev gstreamer1.0-libav \
        libgstrtspserver-1.0-dev gstreamer1.0-tools libges-1.0-dev \
        libharfbuzz-dev liblzma-dev libudev-dev libunwind-dev \
        libvulkan1 libx11-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxmu-dev \
        libxmu6 libegl1-mesa-dev llvm-dev m4 xorg-dev libxkbcommon0 libxkbcommon-x11-0 \
        xvfb gh sudo

RUN mkdir -p /data/servo/servo

FROM base_with_cc AS rust_toolchain
ARG RUST_VERSION=1.82.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain ${RUST_VERSION} \
        --profile=minimal \
        --component rustfmt clippy cargo llvm-tools rustc-dev rustdocs


RUN /root/.cargo/bin/cargo install cargo-chef taplo-cli cargo-deny --locked

##### FETCH SERVO ####################################################################
FROM base_fetcher AS servo_fetcher

RUN mkdir -p /data/servo/servo
# Clone servo, ignoring the history. We keep a small history, just in case some of our
# scripts might rely on a bit of history being available.
RUN cd /data/servo && git clone https://github.com/servo/servo.git --depth 10
WORKDIR /data/servo/servo

# The above layer will be cached. For faster image rebuilds we
# just pull the diff from the previous cached layer.
# When building this image, inject `SERVO_GIT_HASH` as a build-arg to force updating this layer
ARG SERVO_GIT_HASH=origin/main
RUN git pull && git reset --hard "${SERVO_GIT_HASH}"

##### END FETCH SERVO #################################################################

FROM base_fetcher AS uv

ARG UV_VERSION=0.5.10
RUN mkdir -p "$HOME/.local/bin" \
    && curl --proto '=https' --tlsv1.2  --fail \
        -LsSf https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-installer.sh | sh

FROM base AS servo_base

WORKDIR /data/servo/servo

COPY --from=rust_toolchain /root/.cargo /root/.cargo
COPY --from=rust_toolchain /root/.rustup /root/.rustup
COPY --from=uv /root/.local/bin/uv /root/.local/bin/uvx /root/.cargo/bin/
ENV PATH="/root/.cargo/bin:${PATH}"

COPY --from=servo_fetcher /data/servo /data/servo

# ideally this should be a no-op: Todo: investigate the layer and see if it is.
RUN ./mach bootstrap --skip-platform
# Create a layer which prefetches git dependencies
RUN cargo update -p mozjs && git checkout Cargo.lock

########################################################################################
# Cache Layers #########################################################################
# Below we define several `servo_cooked` layers, which are intended as the final
# artifacts for use in CI, with prebuilt servo.
# Build by running `docker build --target servo_cooked_XXX -t servo_ci_XXX:latest
# Note: The max layer size is 10GiB
########################################################################################

# FROM servo_base AS servo_cooked_dev-crown-default_features

# RUN ./mach build --dev --use-crown

# FROM servo_base AS servo_cooked_release-crown-default_features

# RUN ./mach build --release --use-crown

FROM servo_base AS planner

RUN cargo chef prepare --recipe-path recipe.json


FROM servo_base AS servo_cooked_dev-crown-default_features
COPY --from=planner /data/servo/servo/recipe.json recipe.json
# Todo: it would be better if we could run `cargo chef` through `./mach exec xxx`, and have
# mach set the regular environment variables (e.g. CC) to reduce rebuild reasons!
RUN CARGO_BUILD_RUSTC=crown RUSTFLAGS=--cfg=crown cargo chef cook --profile=dev

FROM servo_base AS servo_cooked_release-crown-default_features
COPY --from=planner /data/servo/servo/recipe.json recipe.json
# Todo: it would be better if we could run `cargo chef` through `./mach exec xxx`, and have
# mach set the regular environment variables (e.g. CC) to reduce rebuild reasons!
RUN CARGO_BUILD_RUSTC=crown RUSTFLAGS=--cfg=crown cargo chef cook --profile=release --features layout_2013

#
#FROM servo_base AS servo_cooked_release
#
#COPY --from=planner /data/servo/servo/recipe.json recipe.json
#RUN cargo chef cook --release --recipe-path recipe.json
