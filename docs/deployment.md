# Deployment: One Docker Image, Two Runtimes (Fargate + Lambda)

> **Status: target / planned design.** LTSearch already ships its serving path as a **Lambda
> container image**. This document specifies the intended unified topology so that **one image per
> component runs unchanged on both AWS Lambda and AWS Fargate**. It is documentation of the target;
> the code, Dockerfile, and infrastructure-template changes are follow-up work and are not yet
> implemented. See `docs/arch.md` §22 for the architectural summary.

## Why Docker, and why both runtimes

- **Model size.** The embedding assets baked into the image are ~140 MB
  (`model.ort` ~118 MB + `tokenizer.json` ~16 MB + `libonnxruntime.so` ~4.6 MB). That exceeds the
  Lambda **Layer** limit, so a **container image** is required (already the case). Container images
  fit comfortably under the 10 GB Lambda image limit.
- **Lambda** is ideal for spiky, event-driven traffic and the batch index builder.
- **Fargate** gives an always-on service that loads the model **once per task** (no per-invoke cold
  start of a ~118 MB ONNX model) and removes the 15-minute / 10 GB `/tmp` ceilings that constrain
  the index builder on Lambda.

The goal is to avoid maintaining two artifacts: the **same image** should run in both places.

## Mechanism: AWS Lambda Web Adapter

Each component becomes a plain **HTTP server** listening on `0.0.0.0:8080`. The transport-agnostic
request cores already exist and are reused unchanged:

| Component | HTTP surface (target) | Reused core |
| --- | --- | --- |
| query | `POST /query`, `GET /health` | `handle_search_request` (`src/query_lambda.rs`) |
| write | `POST /write`, `POST /delete`, `GET /health` | `handle_write_request` (`src/write_lambda.rs`) |
| index_builder | `POST /build`, `GET /health` | `handle_build_request` (`src/build_lambda.rs`) |

The [AWS Lambda Web Adapter](https://github.com/awslabs/aws-lambda-web-adapter) is baked into each
image as a Lambda extension:

```dockerfile
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.x /lambda-adapter /opt/extensions/lambda-adapter
```

- **On Lambda**: the extension starts the app, waits for `AWS_LWA_READINESS_CHECK_PATH` to return
  200, then bridges each API Gateway / Function URL / SQS event to `http://localhost:8080`.
- **On Fargate/ECS**: `/opt/extensions/*` is never read; the container just runs the HTTP server.
  ECS health checks hit `GET /health` directly.

### Web Adapter environment knobs

| Variable | Value | Notes |
| --- | --- | --- |
| `AWS_LWA_PORT` | `8080` | Port the app listens on |
| `AWS_LWA_READINESS_CHECK_PATH` | `/health` | Adapter waits for 200 before serving |
| `AWS_LWA_INVOKE_MODE` | `buffered` | `response_stream` only if streaming is added |
| `AWS_LWA_PASS_THROUGH_PATH` | `/build` | For SQS-triggered builder: raw event POSTed here |

## Image structure

Reuse the existing multi-stage build (`sam/builder.Dockerfile` as the shared compile stage), and
change only the runtime base and entrypoint:

```dockerfile
# --- runtime (per component), replacing public.ecr.aws/lambda/provided:al2023-arm64 ---
FROM --platform=linux/arm64 public.ecr.aws/amazonlinux/amazonlinux:2023
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.x /lambda-adapter /opt/extensions/lambda-adapter
COPY --from=builder /<component>_server /app/server        # HTTP server binary
COPY --from=builder /ltembed-assets   /ltembed-assets      # query + index_builder only
# query image also bakes the static TurboQuant index:
# COPY static/ /app/static/     ;  ENV LTSEARCH_QUERY_STATIC_DIR=/app
ENV AWS_LWA_PORT=8080 AWS_LWA_READINESS_CHECK_PATH=/health
EXPOSE 8080
CMD ["/app/server"]
```

This also **unifies the two divergent Dockerfiles** that exist today: the top-level `Dockerfile`
(x86, bakes `static/`, `CMD [bootstrap]`) is folded into the arm64 `sam/` lineage so static-index
baking lives in exactly one place.

## Platform mapping (all three components)

| Component | Fargate (always-on) | Lambda (event-driven) |
| --- | --- | --- |
| query | ECS **service** behind an ALB; desired count scales on CPU/latency | Function behind API Gateway / Function URL |
| write | ECS **service** behind an ALB | Function behind API Gateway / Function URL |
| index_builder | ECS **task** driven by an SQS worker loop (or scheduled) | Function with an SQS **EventSource** mapping |

The **same image** is deployed in either column; only the surrounding infrastructure differs.

## Model assets and architecture portability

- Assets are baked at build time via `sam/builder.Dockerfile` build args:
  `LTEMBED_MODE=real` and `LTEMBED_BUNDLE_URL` (pinned default: `minimal-ort-builder` **v1.0.9**,
  `jina-embeddings-v5-text-nano-retrieval` q4f16 **linux-arm64**). The default `LTEMBED_MODE=stub`
  builds the `fixed` provider with no model (CI default).
- Runtime lookup is via env: `LTSEARCH_{QUERY,BUILD}_LTEMBED_BUNDLE_DIR=/ltembed-assets` and
  `LTSEARCH_{QUERY,BUILD}_LTEMBED_MODEL_PATH=/ltembed-assets/model.ort`. `libonnxruntime.so` is
  resolved from the bundle dir (or `ORT_DYLIB_PATH`).
- **Architecture must match.** `ort` uses `load-dynamic`, so the compiled binary + the bundled
  `.so` are portable **only within the same CPU arch**. The pinned bundle is arm64, so both Fargate
  and Lambda must run on **arm64 (Graviton)**. Targeting x86_64 Fargate requires an x86_64
  `minimal-ort-builder` bundle and a matching `LTEMBED_BUNDLE_URL`.

## Runtime environment variables (unchanged by the runtime bridge)

| Component | Key variables |
| --- | --- |
| query | `LTSEARCH_QUERY_S3_BUCKET`, `LTSEARCH_QUERY_ARTIFACT_ROOT`, `LTSEARCH_QUERY_EMBEDDING_PROVIDER`, `LTSEARCH_QUERY_LTEMBED_*`, `LTSEARCH_QUERY_STATIC_DIR` |
| write | `LTSEARCH_WRITE_S3_BUCKET`, `LTSEARCH_WRITE_SQS_QUEUE_URL` |
| index_builder | `LTSEARCH_BUILD_S3_BUCKET`, `LTSEARCH_BUILD_ARTIFACT_ROOT`, `LTSEARCH_BUILD_EMBEDDING_PROVIDER`, `LTSEARCH_BUILD_EMBEDDING_DIM`, `LTSEARCH_BUILD_LTEMBED_*` |

On Fargate, prefer an EFS mount or a large task ephemeral volume for `*_ARTIFACT_ROOT` so synced S3
artifacts persist for the life of the task.

## Open items before implementation

- Add the HTTP server entrypoints (thin axum wrappers around the existing `handle_*` cores).
- Wire the SQS → build trigger and version allocation (see `docs/design.md` → Known Gaps #1).
- Provide the ECS task definitions / Lambda SAM resources for both columns.
- Publish images to ECR (no push/deploy job exists in CI today — everything is local `sam local`).
