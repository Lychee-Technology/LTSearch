FROM ltsearch-e2e-builder AS builder

FROM public.ecr.aws/lambda/provided:al2023-arm64
COPY --from=builder /index_builder_lambda /var/runtime/bootstrap
COPY --from=builder /ltembed-assets /ltembed-assets
