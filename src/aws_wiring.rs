//! index_builder 的 AWS 接线闭包：build（读全部 WAL 段 → embedding → 建索引）、
//! publish（CAS 发布）、WAL 段列举、embedding 健康 probe。原先内联在已退役的
//! index_builder_server bin 中（#113 删除），抽到 lib 后由 index_builder_lambda
//! 复用（#109：SQS 事件触发的 builder lambda 复用 process_queue_message 全链路）。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::FutureExt;

use crate::adapters::s3_publish::AwsPublishStorage;
use crate::adapters::s3_wal::AwsS3WalStorage;
use crate::bootstrap::{
    build_embedding_generator_from_env, build_embedding_provider_from_env,
    probe_build_embedding_from_env, BuildConfig,
};
use crate::build_worker::ListWalKeysFn;
use crate::error::IndexError;
use crate::http::build::SnapshotBuildRequest;
use crate::indexing::{BuildIndexRequest, IndexPublisher, LocalIndexBuilder, PublishRequest};
use crate::write::WriteAheadLog;

/// build 闭包：按序读取全部 `wal_keys` → 构建 embedding 引擎 →
/// LocalIndexBuilder（spawn_blocking）。WAL 读取路径与
/// src/bin/index_builder_lambda.rs:51-86 一致，仅从单段扩展为多段拼接——快照
/// 重放的 last-wins 语义由 `materialize_latest_snapshot` 按记录 timestamp 保证。
pub fn build_closure(
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
) -> crate::http::build::BuildFn {
    Arc::new(move |request: SnapshotBuildRequest| {
        let config = config.clone();
        let s3_client = s3_client.clone();
        async move {
            let wal_storage = AwsS3WalStorage::new(config.s3_bucket.clone(), s3_client.clone());
            let wal = WriteAheadLog::new(wal_storage);
            let mut records = Vec::new();
            for wal_key in &request.wal_keys {
                let segment = wal
                    .read(wal_key)
                    .await
                    .map_err(|error| IndexError::Operation {
                        message: format!("failed to read WAL records from {wal_key}: {error}"),
                    })?;
                records.extend(segment);
            }

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
pub fn publish_closure(
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
) -> crate::http::build::PublishFn {
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

/// WAL 段列举闭包：ListObjectsV2 分页列出 `wal/` 前缀下全部对象 key，供 worker
/// 在每次构建前取得完整快照输入。单用户规模段数有限，不做增量/缓存。
pub fn list_wal_keys_closure(bucket: String, s3_client: aws_sdk_s3::Client) -> ListWalKeysFn {
    Arc::new(move || {
        let bucket = bucket.clone();
        let s3_client = s3_client.clone();
        async move {
            let mut keys = Vec::new();
            let mut paginator = s3_client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(crate::write::WAL_PREFIX)
                .into_paginator()
                .send();
            while let Some(page) = paginator.next().await {
                let page = page
                    .map_err(|error| format!("failed to list WAL objects in {bucket}: {error}"))?;
                for object in page.contents() {
                    if let Some(key) = object.key() {
                        keys.push(key.to_string());
                    }
                }
            }
            Ok(keys)
        }
        .boxed()
    })
}

pub fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
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
