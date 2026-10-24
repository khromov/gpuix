# A Linux build environment for gpuix-qjs, for trying the Linux path from a Mac:
#
#   docker build -t gpuix-qjs-linux packages/qjs/docker
#   docker run --rm -v "$PWD":/work -v gpuix-qjs-target:/target -v gpuix-qjs-cargo:/usr/local/cargo/registry \
#     -e CARGO_TARGET_DIR=/target -w /work/packages/qjs gpuix-qjs-linux cargo build --release
#
# The target dir lives in a volume so it never mixes with the host's macOS build,
# and the registry volume keeps crate downloads across runs. Rust's channel comes
# from the image tag, which must match rust-toolchain.toml. The apt list is the
# one ci.yml installs for the native build (Vulkan, Wayland, X11, fonts), plus
# Xvfb and Mesa's software Vulkan driver so the result can open a window headlessly.
FROM rust:1.97.1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc g++ clang cmake pkg-config \
    libfontconfig-dev \
    libwayland-dev \
    libxkbcommon-x11-dev \
    libx11-xcb-dev \
    libssl-dev \
    libzstd-dev \
    libvulkan1 \
    vulkan-validationlayers \
    mesa-vulkan-drivers \
    xvfb xauth x11-utils imagemagick \
    fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

# rust-toolchain.toml in the mounted checkout would otherwise make rustup try to
# install the channel again inside the container.
ENV RUSTUP_TOOLCHAIN=1.97.1
