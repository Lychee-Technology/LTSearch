FROM ltsearch-e2e-builder AS builder

FROM public.ecr.aws/lambda/provided:al2023-arm64
COPY --from=builder /query_lambda /var/runtime/bootstrap
