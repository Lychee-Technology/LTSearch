//! index_builder 的 HTTP 服务进程：暴露 `POST /build` + `GET /health`，并在
//! 设置了 `LTSEARCH_BUILD_SQS_QUEUE_URL` 时后台轮询构建队列。build/publish
//! 接线照抄 src/bin/index_builder_lambda.rs:50-101，抽成 BuildServerState 的两个
//! 闭包供 HTTP handler 与 SQS worker 共用。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::FutureExt;

use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::bootstrap::{
    build_embedding_generator_from_env, build_embedding_provider_from_env,
    probe_build_embedding_from_env, s3_client_from_env, sqs_client_from_env, BuildConfig,
};
use ltsearch::build_lambda::BuildRequest;
use ltsearch::build_worker::run_sqs_worker_loop;
use ltsearch::error::IndexError;
use ltsearch::http::build::{build_router, BuildServerState};
use ltsearch::http::{port_from_env, serve};
use ltsearch::indexing::{BuildIndexRequest, IndexPublisher, LocalIndexBuilder, PublishRequest};
use ltsearch::write::WriteAheadLog;

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
                ));
            }
        }

        let port = port_from_env();
        eprintln!("ltsearch-index-builder-server listening on 0.0.0.0:{port}");
        serve(build_router(state), port).await?;
        Ok(())
    })
}

/// build 闭包：读 WAL → 构建 embedding 引擎 → LocalIndexBuilder（spawn_blocking）。
/// 照抄 src/bin/index_builder_lambda.rs:51-86。
fn build_closure(
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
) -> ltsearch::http::build::BuildFn {
    Arc::new(move |request: BuildRequest| {
        let config = config.clone();
        let s3_client = s3_client.clone();
        async move {
            let wal_storage = AwsS3WalStorage::new(config.s3_bucket.clone(), s3_client.clone());
            let wal = WriteAheadLog::new(wal_storage);
            let records =
                wal.read(&request.wal_key)
                    .await
                    .map_err(|error| IndexError::Operation {
                        message: format!("failed to read WAL records: {error}"),
                    })?;

            let provider =
                build_embedding_provider_from_env().map_err(|error| IndexError::Operation {
                    message: error.to_string(),
                })?;
            let embedding_generator =
                build_embedding_generator_from_env(provider).map_err(|error| {
                    IndexError::Operation {
                        message: error.to_string(),
                    }
                })?;

            // The build is sync + CPU-heavy, so run it off the async runtime.
            let builder = LocalIndexBuilder::new(&config.artifact_root, embedding_generator);
            let build_request = BuildIndexRequest {
                version_id: request.version_id,
                created_at: current_time_millis(),
                embedding_dim: request.embedding_dim,
                records,
            };
            tokio::task::spawn_blocking(move || builder.build(&build_request))
                .await
                .map_err(|error| IndexError::Operation {
                    message: format!("build task panicked: {error}"),
                })?
        }
        .boxed()
    })
}

/// publish 闭包：AwsPublishStorage → IndexPublisher.publish；`expected` 由调用方
/// 注入（HTTP 侧 None，worker 侧观测到的 head）。照抄
/// src/bin/index_builder_lambda.rs:87-98，仅把 expected_current_version 参数化。
fn publish_closure(
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
) -> ltsearch::http::build::PublishFn {
    Arc::new(move |manifest, expected: Option<u64>| {
        let config = config.clone();
        let s3_client = s3_client.clone();
        async move {
            let publish_storage =
                AwsPublishStorage::new(config.s3_bucket.clone(), s3_client.clone());
            let publisher = IndexPublisher::new(&config.artifact_root, publish_storage);
            publisher
                .publish(&PublishRequest {
                    manifest,
                    expected_current_version: expected,
                    updated_at: current_time_millis(),
                })
                .await
        }
        .boxed()
    })
}

fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
    use std::sync::OnceLock;
    static PROBE_RESULT: OnceLock<Result<usize, String>> = OnceLock::new();
    move || {
        PROBE_RESULT
            .get_or_init(probe_build_embedding_from_env)
            .clone()
    }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
