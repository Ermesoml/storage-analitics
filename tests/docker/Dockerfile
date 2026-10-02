FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git musl-tools pkg-config python3 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.96.0 \
    && rustup component add rustfmt clippy \
    && rustup target add "$(uname -m)-unknown-linux-musl" \
    && rm /tmp/rustup.sh

WORKDIR /workspace
CMD ["bash"]
