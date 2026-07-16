//! index_builder 的 HTTP 服务进程：暴露 `POST /build` + `GET /health`，并在
//! 设置了 `LTSEARCH_BUILD_SQS_QUEUE_URL` 时后台轮询构建队列。build/publish
//! 接线照抄 src/bin/index_builder_lambda.rs:50-101，抽成 BuildServerState 的两个
//! 闭包供 HTTP handler 与 SQS worker 共用。

use std::sync::Arc;

use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::aws_wiring::{
    build_closure, build_embedding_probe, list_wal_keys_closure, publish_closure,
};
use ltsearch::bootstrap::{s3_client_from_env, sqs_client_from_env, BuildConfig};
use ltsearch::build_worker::run_sqs_worker_loop;
use ltsearch::http::build::{build_router, BuildServerState};
use ltsearch::http::{port_from_env, serve};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let config = BuildConfig::from_env()?;
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let s3_client = s3_client_from_env(&sdk_config);

        let state = BuildServerState {
            build: build_closure(config.clone(), s3_client.clone()),
            publish: publish_closure(config.clone(), s3_client.clone()),
            // 启动时构建一次 probe 闭包；probe 本身按调用惰性初始化 embedding
            // 引擎，避免模型损坏导致进程退出——健康检查需以 503 报告细节。
            embedding_probe: Arc::new(build_embedding_probe()),
        };

        // 设置了队列 URL 才启用 worker；否则仅提供 HTTP（例如仅按需 POST /build）。
        if let Ok(queue_url) = std::env::var("LTSEARCH_BUILD_SQS_QUEUE_URL") {
            if !queue_url.trim().is_empty() {
                let sqs = sqs_client_from_env(&sdk_config);
                let publish_storage =
                    AwsPublishStorage::new(config.s3_bucket.clone(), s3_client.clone());
                let worker_state = state.clone();
                eprintln!("ltsearch-index-builder-server: SQS worker enabled on {queue_url}");
                tokio::spawn(run_sqs_worker_loop(
                    sqs,
                    queue_url,
                    worker_state,
                    publish_storage,
                    list_wal_keys_closure(config.s3_bucket.clone(), s3_client.clone()),
                ));
            }
        }

        let port = port_from_env();
        eprintln!("ltsearch-index-builder-server listening on 0.0.0.0:{port}");
        serve(build_router(state), port).await?;
        Ok(())
    })
}
