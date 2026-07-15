//! local profile 构造证明：用文件系统/内存契约实现组装出 write / build / query
//! 三条链路所需的核心类型，断言构造成功且不触及任何 AWS。
#![cfg(feature = "local")]

use ltsearch::contracts::{ArtifactSync, BuildJobSource, PublishStorage, WalStorage};
use ltsearch::local::{
    LocalFsBuildQueue, LocalFsPublishStorage, LocalFsWalStorage, NoopArtifactSync,
};
use ltsearch::storage::LocalManifestStore;

#[tokio::test]
async fn local_profile_constructs_all_four_contract_families() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // document events
    let wal = LocalFsWalStorage::new(root);
    wal.append("wal/2026/07/14/batch-1.jsonl", b"{}\n")
        .await
        .unwrap();

    // build jobs (producer + consumer are the same local type)
    let queue = LocalFsBuildQueue::new(root);
    let jobs = queue.receive().await.unwrap();
    assert!(jobs.is_empty());

    // artifact access
    let publish = LocalFsPublishStorage::new(root);
    assert!(publish
        .compare_and_swap("index/_head", None, b"seed")
        .await
        .unwrap());
    let sync = NoopArtifactSync::new();
    sync.sync(root).await.unwrap();

    // active-release coordination
    let _manifest_store = LocalManifestStore::new(root);
}
