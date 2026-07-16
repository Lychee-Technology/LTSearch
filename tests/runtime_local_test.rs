//! local profile 构造证明：用 SQLite 后端 + 文件系统制品组装出 write / build / query
//! 三条链路所需的核心契约实现，断言构造与基本读写成功且不触及任何 AWS。
#![cfg(feature = "local")]

use ltsearch::contracts::{ArtifactSync, BuildJobSource, PublishStorage, WalStorage};
use ltsearch::local::{
    LocalPublishStorage, NoopArtifactSync, SqliteBuildJobSource, SqliteBuildQueue, SqliteDb,
    SqliteManifestStore, SqliteWalStorage,
};
use ltsearch::write::{BuildQueue, QueueBatch};

#[tokio::test]
async fn local_profile_constructs_all_four_contract_families() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 单一 SQLite 库承载耐久事件 / 队列 / 活跃指针；WAL 与队列共用同一 SqliteDb。
    let db = SqliteDb::open(root.join("ltsearch.db")).unwrap();

    // document events
    let wal = SqliteWalStorage::new(db.clone());
    wal.append("wal/2026/07/14/batch-1.jsonl", b"{}\n")
        .await
        .unwrap();

    // build jobs (producer + consumer share the DB)
    let queue = SqliteBuildQueue::new(db.clone());
    queue
        .enqueue(QueueBatch {
            batch_id: "batch-1".to_string(),
            wal_key: "wal/2026/07/14/batch-1.jsonl".to_string(),
            accepted_count: 1,
            wal_event_ids: vec!["batch-1-000001".to_string()],
        })
        .await
        .unwrap();
    let source = SqliteBuildJobSource::new(db.clone());
    let jobs = source.receive().await.unwrap();
    assert_eq!(jobs.len(), 1);

    // artifact access: head-CAS via SQLite, artifact bytes via filesystem; sync is a no-op
    let publish = LocalPublishStorage::new(db.clone(), root);
    assert!(publish
        .compare_and_swap("index/_head", None, b"seed")
        .await
        .unwrap());
    let sync = NoopArtifactSync::new();
    sync.sync(root).await.unwrap();

    // active-release coordination
    let _manifest_store = SqliteManifestStore::new(db, root);
}
