//! build worker 的可单测部分：SQS 消息体解析（忽略多余字段）、head+1 版本
//! 分配（读到 `_head` → head+1，未读到 → 1）与快照输入组装（消息只是触发器，
//! 构建必须覆盖列举出的**全部** WAL 段）。轮询循环本身依赖真实 SQS，留待
//! compose e2e 覆盖；此处只固化纯逻辑，不触碰 AWS。

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::FutureExt;
use ltsearch::build_worker::{
    next_version_id, process_queue_message, run_build_job_loop_once, snapshot_wal_keys,
    ListWalKeysFn, QueueBuildMessage,
};
use ltsearch::contracts::{BuildJob, BuildJobSource};
use ltsearch::error::{IndexError, PublishError};
use ltsearch::http::build::{BuildServerState, SnapshotBuildRequest};
use ltsearch::indexing::{
    BuildIndexResult, PublishResult, PublishStorage, UploadMode, VersionedObject,
};
use ltsearch::models::{IndexManifest, ShardManifest};
use ltsearch::storage::{ManifestHead, INDEX_HEAD_KEY};

/// 只实现 `read(_head)` 的内存 PublishStorage：`next_version_id` 只读 head，
/// 其余端口在版本分配路径上永不触达。
#[derive(Clone)]
struct StubHeadStorage {
    head: Option<VersionedObject>,
}

#[async_trait]
impl PublishStorage for StubHeadStorage {
    async fn upload_directory(
        &self,
        _key: &str,
        _source: &Path,
        _mode: UploadMode,
    ) -> Result<(), PublishError> {
        unreachable!("next_version_id must not upload")
    }

    async fn upload_file(
        &self,
        _key: &str,
        _source: &Path,
        _mode: UploadMode,
    ) -> Result<(), PublishError> {
        unreachable!("next_version_id must not upload")
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        assert_eq!(key, INDEX_HEAD_KEY, "next_version_id 只应读取 _head");
        Ok(self.head.clone())
    }

    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected_etag: Option<&str>,
        _new_value: &[u8],
    ) -> Result<bool, PublishError> {
        unreachable!("next_version_id must not compare-and-swap")
    }
}

/// 内存 `BuildJobSource`：`receive` 一次性吐出预置作业，之后为空（保证 `_once`
/// 有界终止）；`ack`/`nack` 各自记录，以便断言「成功 ack、失败 nack」的结算语义。
/// 覆写 `nack` 而非沿用默认（默认=ack）——这样才能区分两条结算路径。
struct FakeJobSource {
    pending: Mutex<Vec<BuildJob>>,
    acked: Arc<Mutex<Vec<BuildJob>>>,
    nacked: Arc<Mutex<Vec<(BuildJob, String)>>>,
}

#[async_trait]
impl BuildJobSource for FakeJobSource {
    async fn receive(&self) -> Result<Vec<BuildJob>, String> {
        Ok(std::mem::take(&mut *self.pending.lock().unwrap()))
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        self.acked.lock().unwrap().push(job.clone());
        Ok(())
    }

    async fn nack(&self, job: &BuildJob, error: &str) -> Result<(), String> {
        self.nacked
            .lock()
            .unwrap()
            .push((job.clone(), error.to_string()));
        Ok(())
    }
}

// 结算语义（失败路径）：喂入 body 非法 JSON 的作业，process_queue_message 在解析阶段
// 即失败，build/publish 闭包永不触达；断言 worker 对失败作业调用 nack（不 ack），并带
// 上错误详情——SQLite 侧据此做退避/死信，AWS/local-fs 默认 nack=ack 行为不变。
#[tokio::test]
async fn run_build_job_loop_once_nacks_when_processing_fails() {
    std::env::set_var("LTSEARCH_BUILD_EMBEDDING_DIM", "3");

    let acked = Arc::new(Mutex::new(Vec::new()));
    let nacked = Arc::new(Mutex::new(Vec::new()));
    let source = FakeJobSource {
        pending: Mutex::new(vec![BuildJob {
            receipt: "r-1".to_string(),
            body: "not-json".to_string(),
        }]),
        acked: acked.clone(),
        nacked: nacked.clone(),
    };
    let state = BuildServerState {
        build: Arc::new(|_request: SnapshotBuildRequest| {
            async {
                Err(IndexError::Operation {
                    message: "build closure must not run for a bad-body job".into(),
                })
            }
            .boxed()
        }),
        publish: Arc::new(|_manifest: IndexManifest, _expected| {
            async {
                Err(PublishError::Operation {
                    message: "publish closure must not run for a bad-body job".into(),
                })
            }
            .boxed()
        }),
        embedding_probe: Arc::new(|| Ok(3)),
    };
    let storage = StubHeadStorage { head: None };
    let list_wal_keys: ListWalKeysFn = Arc::new(|| async { Ok(vec![]) }.boxed());

    let settled = run_build_job_loop_once(&source, &state, &storage, &list_wal_keys).await;

    assert_eq!(settled, 1, "失败作业也应完成一次结算（nack）");
    assert!(acked.lock().unwrap().is_empty(), "失败作业不得走 ack");
    let recorded = nacked.lock().unwrap();
    assert_eq!(recorded.len(), 1, "恰好 nack 一条作业");
    assert_eq!(recorded[0].0.receipt, "r-1");
    assert!(!recorded[0].1.is_empty(), "nack 须携带错误详情");
}

// 结算语义（成功路径）：body 合法且 build/publish 成功 → worker 对该作业 ack（不 nack）。
#[tokio::test]
async fn run_build_job_loop_once_acks_on_success() {
    std::env::set_var("LTSEARCH_BUILD_EMBEDDING_DIM", "3");

    let acked = Arc::new(Mutex::new(Vec::new()));
    let nacked = Arc::new(Mutex::new(Vec::new()));
    let source = FakeJobSource {
        pending: Mutex::new(vec![BuildJob {
            receipt: "r-ok".to_string(),
            body: "{\"batch_id\":\"b-1\",\"wal_key\":\"wal/2026/07/01/b-1.jsonl\"}".to_string(),
        }]),
        acked: acked.clone(),
        nacked: nacked.clone(),
    };
    let state = BuildServerState {
        build: Arc::new(|_request: SnapshotBuildRequest| {
            async {
                Ok(BuildIndexResult {
                    manifest: sample_manifest(1),
                    documents: vec![],
                })
            }
            .boxed()
        }),
        publish: Arc::new(|_manifest: IndexManifest, _expected| {
            async {
                Ok(PublishResult {
                    activated_version_id: 1,
                    previous_version_id: None,
                })
            }
            .boxed()
        }),
        embedding_probe: Arc::new(|| Ok(3)),
    };
    let storage = StubHeadStorage { head: None };
    let list_wal_keys: ListWalKeysFn = Arc::new(|| async { Ok(vec![]) }.boxed());

    let settled = run_build_job_loop_once(&source, &state, &storage, &list_wal_keys).await;

    assert_eq!(settled, 1);
    assert!(nacked.lock().unwrap().is_empty(), "成功作业不得走 nack");
    let recorded = acked.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].receipt, "r-ok");
}

fn head_object(version_id: u64) -> VersionedObject {
    let head = ManifestHead::new(version_id, 1_700_000_000_000);
    VersionedObject {
        bytes: head.to_json_pretty(),
        etag: "\"etag-head\"".into(),
    }
}

// SQS 消息体来自 AwsSqsBuildQueue 序列化的 QueueBatch（batch_id + wal_key +
// 其它字段），worker 只关心两个字段，多余字段必须被 serde 忽略。
#[test]
fn queue_build_message_parses_body_from_sqs_batch() {
    let message: QueueBuildMessage =
        serde_json::from_str(r#"{"batch_id":"b-1","wal_key":"wal/x.jsonl","extra":"ignored"}"#)
            .expect("expected queue message to parse and ignore unknown fields");
    assert_eq!(message.batch_id, "b-1");
    assert_eq!(message.wal_key, "wal/x.jsonl");
}

// 无 _head（首次导入）→ 版本从 1 起，无前任版本。
#[tokio::test]
async fn next_version_id_starts_at_one_when_no_head() {
    let storage = StubHeadStorage { head: None };
    let (version_id, previous) = next_version_id(&storage).await.expect("next_version_id");
    assert_eq!(version_id, 1);
    assert_eq!(previous, None);
}

// _head 指向版本 7 → 分配 8，并回报 7 作为 CAS 的 expected_current_version。
#[tokio::test]
async fn next_version_id_is_head_plus_one_when_head_present() {
    let storage = StubHeadStorage {
        head: Some(head_object(7)),
    };
    let (version_id, previous) = next_version_id(&storage).await.expect("next_version_id");
    assert_eq!(version_id, 8);
    assert_eq!(previous, Some(7));
}

// list 结果排序去重，且消息自带段即使未被 list 到也必须并入快照输入。
#[test]
fn snapshot_wal_keys_merges_sorts_and_dedups() {
    let keys = snapshot_wal_keys(
        vec![
            "wal/2026/07/02/batch-b.jsonl".into(),
            "wal/2026/07/01/batch-a.jsonl".into(),
            "wal/2026/07/02/batch-b.jsonl".into(),
        ],
        "wal/2026/07/03/batch-c.jsonl",
    );
    assert_eq!(
        keys,
        vec![
            "wal/2026/07/01/batch-a.jsonl".to_string(),
            "wal/2026/07/02/batch-b.jsonl".to_string(),
            "wal/2026/07/03/batch-c.jsonl".to_string(),
        ]
    );

    let keys = snapshot_wal_keys(
        vec!["wal/2026/07/01/batch-a.jsonl".into()],
        "wal/2026/07/01/batch-a.jsonl",
    );
    assert_eq!(keys, vec!["wal/2026/07/01/batch-a.jsonl".to_string()]);
}

fn sample_manifest(version_id: u64) -> IndexManifest {
    IndexManifest {
        version_id,
        created_at: 1_700_000_000_000,
        embedding_dim: 3,
        document_count: 1,
        num_shards: 1,
        shards: vec![ShardManifest {
            shard_id: 0,
            document_count: 1,
            lance_path: format!("s3://bucket/lance/v{version_id}/shard_0"),
            tantivy_path: format!("s3://bucket/index/v{version_id}/shard_0"),
        }],
    }
}

// 多写回归（对应 PR #105 P1）：worker 处理某条消息时，build 输入必须是列举出的
// 全部 WAL 段（含消息触发段），而不是只有消息里的单段——每个版本都是全量快照，
// 单段构建会把先前批次的文档挤出新发布的 head。
#[tokio::test]
async fn process_queue_message_builds_snapshot_from_all_listed_wal_segments() {
    std::env::set_var("LTSEARCH_BUILD_EMBEDDING_DIM", "3");

    let captured: Arc<Mutex<Option<SnapshotBuildRequest>>> = Arc::new(Mutex::new(None));
    let captured_in_build = captured.clone();
    let state = BuildServerState {
        build: Arc::new(move |request: SnapshotBuildRequest| {
            let version_id = request.version_id;
            *captured_in_build.lock().unwrap() = Some(request);
            async move {
                Ok(BuildIndexResult {
                    manifest: sample_manifest(version_id),
                    documents: vec![],
                })
            }
            .boxed()
        }),
        publish: Arc::new(|manifest: IndexManifest, _expected| {
            async move {
                Ok(PublishResult {
                    activated_version_id: manifest.version_id,
                    previous_version_id: None,
                })
            }
            .boxed()
        }),
        embedding_probe: Arc::new(|| Ok(3)),
    };
    let storage = StubHeadStorage { head: None };
    let list_wal_keys: ListWalKeysFn = Arc::new(|| {
        async {
            Ok(vec![
                "wal/2026/07/02/batch-second.jsonl".into(),
                "wal/2026/07/01/batch-first.jsonl".into(),
            ])
        }
        .boxed()
    });

    let body = r#"{"batch_id":"batch-second","wal_key":"wal/2026/07/02/batch-second.jsonl"}"#;
    let version = process_queue_message(&state, &storage, &list_wal_keys, body)
        .await
        .expect("process_queue_message");
    assert_eq!(version, 1);

    let request = captured
        .lock()
        .unwrap()
        .clone()
        .expect("build closure must run");
    assert_eq!(request.batch_id, "batch-second");
    assert_eq!(
        request.wal_keys,
        vec![
            "wal/2026/07/01/batch-first.jsonl".to_string(),
            "wal/2026/07/02/batch-second.jsonl".to_string(),
        ],
        "快照输入必须覆盖全部 WAL 段（排序后），而非只有触发消息的单段"
    );
}
