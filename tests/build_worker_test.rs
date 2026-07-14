//! build worker 的可单测部分：SQS 消息体解析（忽略多余字段）与 head+1 版本
//! 分配（读到 `_head` → head+1，未读到 → 1）。轮询循环本身依赖真实 SQS，留待
//! Task 7 的 compose e2e 覆盖；此处只固化纯逻辑，不触碰 AWS。

use std::path::Path;

use async_trait::async_trait;
use ltsearch::build_worker::{next_version_id, QueueBuildMessage};
use ltsearch::error::PublishError;
use ltsearch::indexing::{PublishStorage, UploadMode, VersionedObject};
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
