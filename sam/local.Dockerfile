# 单镜像本地（AWS-free）运行时（#125）：一个镜像四个角色，Compose 以
# `command: ["write"|"build"|"query"|"static-build"]` 选择子命令。
#
# **自包含**：builder stage 就地构建 `ltsearch`（`--features local`），干净环境
# 直接 `docker build --platform linux/arm64 -f sam/local.Dockerfile .` 即可出镜像，不依赖任何预构建的
# builder 镜像（#125 验收标准）。#113 起本文件即发布镜像：release workflow 按
# 原样构建并推送 ghcr.io/lychee-technology/ltsearch-local。
# 工具链设置对齐 sam/builder.Dockerfile；cache mount id 也一致以共享本机缓存。
#
# 无 Lambda Web Adapter（本地部署不过 LWA）、无 CMD（角色由编排方注入）。
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS builder
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /src
COPY . .
# local feature 不启用 ltembed，但仍以 vendored stub patch 固定 LTEmbed git 依赖，
# 避免干净环境解析工作区时触碰私有 git 仓库（与 builder.Dockerfile stub 分支一致）。
RUN printf '\n[patch."https://github.com/Lychee-Technology/LTEmbed"]\nltembed = { path = "/src/vendor/ltembed-stub" }\n' >> /src/.cargo/config.toml
RUN --mount=type=cache,id=ltsearch-cargo-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=ltsearch-cargo-git,target=/root/.cargo/git \
    --mount=type=cache,id=ltsearch-cargo-target,target=/src/target \
    cargo build --release --no-default-features --features local --bin ltsearch && \
    cp target/release/ltsearch /ltsearch

FROM public.ecr.aws/amazonlinux/amazonlinux:2023
COPY --from=builder /ltsearch /app/ltsearch
ENV LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/app/ltsearch"]
