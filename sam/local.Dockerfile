# 单镜像本地（AWS-free）运行时（#125）：一个镜像四个角色，Compose 以
# `command: ["write"|"build"|"query"|"static-build"]` 选择子命令。
#
# 与 *_server.Dockerfile 的差异：无 Lambda Web Adapter（本地部署不过 LWA）、
# 无 CMD（角色由编排方注入）。#113 发布到 GHCR 时按原样采用本文件。
# 前置：`docker build -f sam/builder.Dockerfile -t ltsearch-e2e-builder .`（stub 分支
# 产出 /ltsearch，`--features local`，AWS-free）。
FROM ltsearch-e2e-builder AS builder

FROM --platform=linux/arm64 public.ecr.aws/amazonlinux/amazonlinux:2023
COPY --from=builder /ltsearch /app/ltsearch
ENV LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/app/ltsearch"]
