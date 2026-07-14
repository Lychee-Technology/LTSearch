//! SQS 轮询 worker：从构建队列长轮询取消息，按 `_head` 分配 head+1 版本，复用
//! `POST /build` 的 `run_build` 执行「读 WAL → 建索引 → CAS 发布」。本地单用户
//! 场景不做毒消息隔离：无论成功失败都 `delete_message` 并把失败详情完整落
//! stderr。发布 CAS 冲突（并发别处推进了 head）时重读 head 重试一次。
//!
//! 轮询循环本身依赖真实 SQS，留给 Task 7 的 compose e2e；本模块把可测逻辑
//! （消息解析、版本分配）拆成独立函数以便不依赖 AWS 单测。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::build_lambda::BuildLambdaError;
use crate::http::build::{run_build, BuildServerState, SnapshotBuildRequest};
use crate::indexing::PublishStorage;
use crate::storage::{ManifestHead, INDEX_HEAD_KEY};

/// 列出 `wal/` 前缀下全部 WAL 段的闭包：bin 侧接 S3 ListObjectsV2，测试注入
/// 内存 stub。每个版本都是全量快照，worker 必须基于**全部**段构建；只用消息里
/// 的单段会让后续 write 发布的新 head 丢掉先前批次的文档。
pub type ListWalKeysFn =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<String>, String>> + Send + Sync>;

/// 对应 `AwsSqsBuildQueue` 发出的 `QueueBatch` body；worker 只关心这两个字段，
/// serde 默认忽略多余字段（accepted_count / wal_event_ids 等）。
#[derive(Debug, Clone, Deserialize)]
pub struct QueueBuildMessage {
    pub batch_id: String,
    pub wal_key: String,
}

/// 读 `_head` 分配下一个版本号：存在 → `(head+1, Some(head))`（后者作为发布的
/// `expected_current_version`）；不存在 → `(1, None)`。泛型化以便注入内存
/// storage 单测，无需真实 S3。
pub async fn next_version_id<S: PublishStorage>(storage: &S) -> Result<(u64, Option<u64>), String> {
    let head_object = storage
        .read(INDEX_HEAD_KEY)
        .await
        .map_err(|error| format!("failed to read index head: {error}"))?;
    match head_object {
        None => Ok((1, None)),
        Some(object) => {
            let head = ManifestHead::from_json(&object.bytes)
                .map_err(|error| format!("failed to parse index head: {error}"))?;
            Ok((head.version_id + 1, Some(head.version_id)))
        }
    }
}

/// 从 env 读构建维度；缺失或非法即报错——版本已从 head 分配，没有维度无法组装
/// `BuildRequest`。
fn embedding_dim_from_env() -> Result<usize, String> {
    match std::env::var("LTSEARCH_BUILD_EMBEDDING_DIM") {
        Ok(value) => value
            .trim()
            .parse::<usize>()
            .map_err(|error| format!("invalid LTSEARCH_BUILD_EMBEDDING_DIM={value:?}: {error}")),
        Err(_) => Err("missing LTSEARCH_BUILD_EMBEDDING_DIM".to_string()),
    }
}

/// publish 侧 CAS 冲突：并发别处推进了 head，本次分配的版本已过期，需重读 head
/// 重试。build 错误与其它 publish 错误不重试。
fn is_publish_cas_conflict(error: &BuildLambdaError) -> bool {
    error.error_type == "publish_error" && error.message.contains("publish conflict")
}

/// 合并快照输入：list 结果 + 消息自带段（防御 list 遗漏触发段的情况），排序去
/// 重。段名为 `wal/YYYY/MM/DD/batch-<uuid>.jsonl`，字典序保证跨天有序；同日段
/// 间顺序由 uuid 决定，但快照重放（`materialize_latest_snapshot`）按记录
/// timestamp 取最新，段间顺序只影响同毫秒写同 doc_id 的平局——本地单用户场景
/// 可接受。
pub fn snapshot_wal_keys(mut listed: Vec<String>, message_wal_key: &str) -> Vec<String> {
    if !listed.iter().any(|key| key == message_wal_key) {
        listed.push(message_wal_key.to_string());
    }
    listed.sort();
    listed.dedup();
    listed
}

/// 处理单条消息：解析 → list 全部 WAL 段 → 分配版本 → `run_build`（expected=
/// 旧 head）。CAS 冲突时重读 head 再试一次；其余错误直接返回。返回值供调用方
/// 落日志，消息删除与否与成败无关（本地单用户不做重投）。
pub async fn process_queue_message<S: PublishStorage>(
    state: &BuildServerState,
    storage: &S,
    list_wal_keys: &ListWalKeysFn,
    body: &str,
) -> Result<u64, String> {
    let message: QueueBuildMessage = serde_json::from_str(body)
        .map_err(|error| format!("failed to parse queue message: {error}"))?;
    let embedding_dim = embedding_dim_from_env()?;

    let listed = list_wal_keys()
        .await
        .map_err(|error| format!("failed to list WAL segments: {error}"))?;
    let wal_keys = snapshot_wal_keys(listed, &message.wal_key);

    let (version_id, expected) = next_version_id(storage).await?;
    let request = build_request(&message, &wal_keys, version_id, embedding_dim);

    match run_build(state, request, expected).await {
        Ok(response) => Ok(response.activated_version_id),
        Err(error) if is_publish_cas_conflict(&error) => {
            eprintln!(
                "build worker: publish CAS conflict for batch {}, re-reading head and retrying once",
                message.batch_id
            );
            let (version_id, expected) = next_version_id(storage).await?;
            let request = build_request(&message, &wal_keys, version_id, embedding_dim);
            run_build(state, request, expected)
                .await
                .map(|response| response.activated_version_id)
                .map_err(|error| format!("{}: {}", error.error_type, error.message))
        }
        Err(error) => Err(format!("{}: {}", error.error_type, error.message)),
    }
}

fn build_request(
    message: &QueueBuildMessage,
    wal_keys: &[String],
    version_id: u64,
    embedding_dim: usize,
) -> SnapshotBuildRequest {
    SnapshotBuildRequest {
        batch_id: message.batch_id.clone(),
        wal_keys: wal_keys.to_vec(),
        version_id,
        embedding_dim,
    }
}

/// 长轮询循环：`receive_message`（wait 10s、每次 1 条）→ 处理 → 无论成败都
/// `delete_message`，失败大声打 stderr。receive 出错时退避 5s 再试，避免打爆
/// SQS。`storage` 用于版本分配（读 head），`state` 提供 build/publish 接线。
pub async fn run_sqs_worker_loop<S: PublishStorage>(
    sqs: aws_sdk_sqs::Client,
    queue_url: String,
    state: BuildServerState,
    storage: S,
    list_wal_keys: ListWalKeysFn,
) {
    loop {
        let received = sqs
            .receive_message()
            .queue_url(&queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(10)
            .send()
            .await;
        let messages = match received {
            Ok(output) => output.messages.unwrap_or_default(),
            Err(error) => {
                eprintln!("build worker: receive_message failed: {error}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        for message in messages {
            let body = message.body().unwrap_or_default();
            match process_queue_message(&state, &storage, &list_wal_keys, body).await {
                Ok(version_id) => {
                    eprintln!("build worker: published index version {version_id}");
                }
                Err(error) => {
                    // 本地单用户场景不做毒消息隔离：记录完整失败详情后照常删消息。
                    eprintln!(
                        "build worker: message processing failed (message dropped after logging): {error}"
                    );
                }
            }

            if let Some(handle) = message.receipt_handle() {
                if let Err(error) = sqs
                    .delete_message()
                    .queue_url(&queue_url)
                    .receipt_handle(handle)
                    .send()
                    .await
                {
                    eprintln!("build worker: delete_message failed: {error}");
                }
            }
        }
    }
}
