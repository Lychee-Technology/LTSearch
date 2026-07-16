FROM ltsearch-e2e-builder AS builder

FROM public.ecr.aws/amazonlinux/amazonlinux:2023
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
COPY --from=builder /write_server /app/server
ENV AWS_LWA_PORT=8080 \
    AWS_LWA_READINESS_CHECK_PATH=/health \
    LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
CMD ["/app/server"]
