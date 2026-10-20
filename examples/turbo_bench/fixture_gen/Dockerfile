# Build environment for turbo-bench-embed, the real-embedding fixture embedder (#177).
# generate.sh injects the base image, the dnf release lock and the Rust toolchain from
# sam/builder.Dockerfile, so the static llama.cpp archives link against the libstdc++ and
# libm of the Lambda build. An empty value fails the build instead of falling back to a
# second pin.
ARG BASE_IMAGE=
FROM ${BASE_IMAGE}
ARG RELEASEVER=
ARG RUST_TOOLCHAIN=
RUN test -n "$RELEASEVER" && test -n "$RUST_TOOLCHAIN" && \
    echo "$RELEASEVER" > /etc/dnf/vars/releasever
RUN dnf install -y --allowerasing gcc gcc-c++ git tar gzip curl && dnf clean all
# Under /usr/local rather than /root, so generate.sh can run the container as the
# invoking user and the files it writes stay theirs.
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH="/usr/local/cargo/bin:${PATH}"
RUN curl https://sh.rustup.rs -sSf | \
    sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_TOOLCHAIN" && \
    chmod -R a+rX /usr/local/rustup /usr/local/cargo
