//! SQS 触发的 index builder lambda：每条记录即写路径入队的 `QueueBatch`，复用
//! build worker 的 `process_queue_message`（list 全部 WAL 段 → head 分配版本 →
//! run_build，CAS 冲突重试一次）。失败记录以 partial-batch failure 报告，由
//! Lambda event source mapping 决定重投/进 DLQ——本进程绝不手动删消息。

use std::sync::Arc;

use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::aws_wiring::{
    build_closure, build_embedding_probe, list_wal_keys_closure, publish_closure,
};
use ltsearch::bootstrap::{s3_client_from_env, BuildConfig};
use ltsearch::build_worker::{process_queue_message, ListWalKeysFn};
use ltsearch::http::build::BuildServerState;
use ltsearch::lambda_events::{process_sqs_records, SqsBatchResponse, SqsEvent, SqsRecord};
use serde_json::Value;

async fn function_handler(
    state: &BuildServerState,
    storage: &AwsPublishStorage,
    list_wal_keys: &ListWalKeysFn,
    event: LambdaEvent<Value>,
) -> Result<SqsBatchResponse, Error> {
    let (payload, _) = event.into_parts();
    // 信封本身解析失败 = 非 SQS 触发的异常调用：整批报错，交给重投策略。
    let sqs_event: SqsEvent = serde_json::from_value(payload)
        .map_err(|source| Error::from(format!("failed to deserialize SQS event: {source}")))?;

    let response = process_sqs_records(sqs_event, async |record: &SqsRecord| {
        process_queue_message(state, storage, list_wal_keys, &record.body)
            .await
            .map(|version_id| {
                eprintln!("index builder lambda: published index version {version_id}");
            })
    })
    .await;

    Ok(response)
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let config = BuildConfig::from_env()?;
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let s3_client = s3_client_from_env(&sdk_config);

        let state = BuildServerState {
            build: build_closure(config.clone(), s3_client.clone()),
            publish: publish_closure(config.clone(), s3_client.clone()),
            embedding_probe: Arc::new(build_embedding_probe()),
        };
        let storage = AwsPublishStorage::new(config.s3_bucket.clone(), s3_client.clone());
        let list_wal_keys = list_wal_keys_closure(config.s3_bucket.clone(), s3_client.clone());

        lambda_runtime::run(service_fn(move |event| {
            let state = state.clone();
            let storage = storage.clone();
            let list_wal_keys = list_wal_keys.clone();
            async move { function_handler(&state, &storage, &list_wal_keys, event).await }
        }))
        .await
    })
}
