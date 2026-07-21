//! 构建 worker：从构建作业源长轮询取作业，按 `_head` 分配 head+1 版本，复用
//! `POST /build` 的 `run_build` 执行「读 WAL → 建索引 → CAS 发布」。本地单用户
//! 场景不做毒消息隔离：无论成功失败都 `ack`（删除）并把失败详情完整落
//! stderr。发布 CAS 冲突（并发别处推进了 head）时重读 head 重试一次。
//!
//! 轮询循环 [`run_build_job_loop`] 只依赖供应商中立的 [`BuildJobSource`]，不再
//! 直接触碰 SQS：AWS 实现见 `#[cfg(feature = "aws")]` 的 `SqsBuildJobSource`，
//! `run_sqs_worker_loop` 是保留原签名的薄封装。可测逻辑（消息解析、版本分配、
//! 有界的 [`run_build_job_loop_once`]）拆成独立函数以便不依赖 AWS 单测。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::build_lambda::BuildLambdaError;
use crate::contracts::BuildJobSource;
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

/// 后台 worker 开关：缺失/空白/无法识别 → 启用（保持既有自动构建行为），仅显式
/// falsy（`0`/`false`/`no`/`off`，忽略大小写与首尾空白）关闭。禁用后 build 角色
/// 仅服务显式 `POST /build`，避免 worker 与显式请求竞争发版。
pub fn build_worker_enabled_from_env() -> bool {
    parse_worker_enabled(
        std::env::var("LTSEARCH_BUILD_WORKER_ENABLED")
            .ok()
            .as_deref(),
    )
}

fn parse_worker_enabled(raw: Option<&str>) -> bool {
    !matches!(
        raw.map(|value| value.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
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

/// 供应商中立的构建作业循环：只依赖 [`BuildJobSource`]，不再直接触碰 SQS。
/// `receive` 出错时退避 5s 再试，避免打爆后端；`storage` 用于版本分配（读 head），
/// `state` 提供 build/publish 接线。无限循环，交由调用方 `tokio::spawn`。
pub async fn run_build_job_loop<C: BuildJobSource, S: PublishStorage>(
    source: C,
    state: BuildServerState,
    storage: S,
    list_wal_keys: ListWalKeysFn,
) {
    loop {
        let _ = run_build_job_loop_once(&source, &state, &storage, &list_wal_keys).await;
    }
}

/// 有界的一趟：`receive` → 逐条 `process_queue_message` → **无论成败都 `ack`**，
/// 返回已成功「结算」（ack 或 nack 均视为一次成功结算）的作业数。成功发布则 `ack`
/// 删除；处理失败则 `nack`，由 [`BuildJobSource`] 决定后续——SQLite 实现做退避重试与
/// 死信，AWS/local-fs 的默认 `nack` 等价于 `ack`（失败照常删除），因此对 AWS 侧行为
/// 逐字不变。`receive` 失败时退避 5s 并返回 0。
pub async fn run_build_job_loop_once<C: BuildJobSource, S: PublishStorage>(
    source: &C,
    state: &BuildServerState,
    storage: &S,
    list_wal_keys: &ListWalKeysFn,
) -> usize {
    let jobs = match source.receive().await {
        Ok(jobs) => jobs,
        Err(error) => {
            eprintln!("build worker: receive failed: {error}");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            return 0;
        }
    };

    let mut settled = 0;
    for job in jobs {
        let outcome = process_queue_message(state, storage, list_wal_keys, &job.body).await;
        let signal = match outcome {
            Ok(version_id) => {
                eprintln!("build worker: published index version {version_id}");
                source.ack(&job).await
            }
            Err(error) => {
                // 处理失败：交给作业源决定重试/死信（SQLite 退避+DLQ；AWS/local-fs
                // 的默认 nack 即 ack，失败照常删除）。完整错误详情落 stderr。
                eprintln!("build worker: message processing failed, nacking: {error}");
                source.nack(&job, &error).await
            }
        };
        if let Err(error) = signal {
            eprintln!("build worker: outcome signaling failed: {error}");
        } else {
            settled += 1;
        }
    }
    settled
}

/// SQS worker 的薄封装：保持原公开签名（bin 侧无需改动），构造
/// [`SqsBuildJobSource`] 后委托给供应商中立的 [`run_build_job_loop`]。
#[cfg(feature = "aws")]
pub async fn run_sqs_worker_loop<S: PublishStorage>(
    sqs: aws_sdk_sqs::Client,
    queue_url: String,
    state: BuildServerState,
    storage: S,
    list_wal_keys: ListWalKeysFn,
) {
    let source = crate::adapters::sqs_job_source::SqsBuildJobSource::new(sqs, queue_url);
    run_build_job_loop(source, state, storage, list_wal_keys).await;
}

#[cfg(test)]
mod tests {
    use super::parse_worker_enabled;

    /// 默认启用语义：缺失、空白与无法识别的值都不得静默关闭自动构建；HTTP
    /// `/build` 的服务与 worker 无关（router 测试本就在无 worker 下运行），开关
    /// 只决定是否 spawn 后台循环。
    #[test]
    fn worker_enabled_defaults_to_true_for_missing_blank_or_unknown_values() {
        assert!(parse_worker_enabled(None));
        assert!(parse_worker_enabled(Some("")));
        assert!(parse_worker_enabled(Some("   ")));
        assert!(parse_worker_enabled(Some("1")));
        assert!(parse_worker_enabled(Some("true")));
        assert!(parse_worker_enabled(Some("yes")));
        assert!(parse_worker_enabled(Some("garbage")));
    }

    #[test]
    fn worker_disabled_only_by_explicit_falsy_values() {
        assert!(!parse_worker_enabled(Some("0")));
        assert!(!parse_worker_enabled(Some("false")));
        assert!(!parse_worker_enabled(Some("FALSE")));
        assert!(!parse_worker_enabled(Some("no")));
        assert!(!parse_worker_enabled(Some("off")));
        assert!(!parse_worker_enabled(Some(" Off ")));
    }
}
