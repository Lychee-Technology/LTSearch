#![cfg(feature = "aws")]
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_config::retry::RetryConfig;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::index::V3_RELEASE_OUTPUT_FILES;
use ltsearch::indexing::PublishStorage;
use ltsearch::indexing::{
    activate_static_pointer, verify_release_dir, BuildIndexRequest, BuildIndexResult,
    IndexPublisher, LocalIndexBuilder, PublishRequest, UploadMode,
};
use ltsearch::storage::{
    static_release_dir_key, static_release_manifest_key, StaticReleaseHead, STATIC_HEAD_KEY,
};
use ltsearch::write::{BuildQueue, WalStorage};

mod support;
use support::build_v3_release_fixture;

struct MotoHarness {
    artifact_root: std::path::PathBuf,
    bucket: String,
    queue_url: String,
    s3: S3Client,
    sqs: SqsClient,
}

#[tokio::test]
async fn moto_smoke_test_can_create_bucket_and_queue() {
    let harness = MotoHarness::new("bootstrap-smoke").await;
    assert!(harness.bucket_exists().await);
    assert!(harness.queue_exists().await);
}

#[tokio::test]
async fn write_api_ingest_can_be_awaited_in_integration_context() {
    let api = test_write_api();
    let response = api.ingest(Vec::new()).await;
    assert!(response.is_err());
}

#[tokio::test]
async fn index_publisher_publish_can_be_awaited_in_integration_context() {
    let publisher = test_publisher();
    let request = test_publish_request();
    let result = publisher.publish(&request).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn moto_harness_can_construct_all_adapter_types() {
    let harness = MotoHarness::new("adapter-constructors").await;
    let _ = AwsS3WalStorage::new(harness.bucket.clone(), harness.s3.clone());
    let _ = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let _ = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
}

#[tokio::test]
async fn s3_wal_storage_first_append_creates_object() {
    let harness = MotoHarness::new("s3-wal-create").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));

    wal.append_bytes("wal/2023/11/14/batch-test.jsonl", b"line-1\n")
        .await
        .unwrap();

    let stored = harness
        .read_s3_text("wal/2023/11/14/batch-test.jsonl")
        .await;
    assert_eq!(stored, "line-1\n");
}

#[tokio::test]
async fn s3_wal_storage_round_trips_jsonl_bytes_against_moto() {
    let harness = MotoHarness::new("s3-wal-roundtrip").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));
    let key = "wal/2023/11/14/batch-test.jsonl";

    wal.append_bytes(
        key,
        b"{\"event_id\":\"e1\",\"doc_id\":\"doc-1\",\"op\":\"delete\",\"document\":null,\"timestamp\":1700000000000}\n",
    )
    .await
    .unwrap();

    let records = wal.read(key).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].doc_id, "doc-1");
}

#[tokio::test]
async fn sqs_build_queue_enqueues_batch_metadata_against_moto() {
    let harness = MotoHarness::new("sqs-build-queue").await;
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());

    queue
        .enqueue(ltsearch::write::QueueBatch {
            batch_id: "batch-123".into(),
            wal_key: "wal/2023/11/14/batch-123.jsonl".into(),
            accepted_count: 2,
            wal_event_ids: vec!["batch-123-000001".into(), "batch-123-000002".into()],
        })
        .await
        .unwrap();

    let message = harness.receive_one_message_body().await;
    assert!(message.contains("batch-123"));
    assert!(message.contains("wal/2023/11/14/batch-123.jsonl"));
}

#[tokio::test]
async fn publish_storage_uploads_and_reads_manifest_bytes() {
    let harness = MotoHarness::new("publish-storage-read").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let artifact_root = harness.new_artifact_root();
    let manifest_path = artifact_root.join("index/versions/7/manifest.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(&manifest_path, b"{}\n").unwrap();

    storage
        .upload_file(
            "index/versions/7/manifest.json",
            &manifest_path,
            ltsearch::indexing::UploadMode::CreateOnly,
        )
        .await
        .unwrap();
    assert!(storage
        .read("index/versions/7/manifest.json")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn publish_storage_create_only_upload_refuses_to_overwrite() {
    let harness = MotoHarness::new("publish-storage-create-only").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let artifact_root = harness.new_artifact_root();
    let file_path = artifact_root.join("shard.bin");
    std::fs::create_dir_all(&artifact_root).unwrap();
    std::fs::write(&file_path, b"v1").unwrap();

    storage
        .upload_file(
            "lance/v7/shard_0/shard.bin",
            &file_path,
            ltsearch::indexing::UploadMode::CreateOnly,
        )
        .await
        .unwrap();

    std::fs::write(&file_path, b"v2").unwrap();
    let error = storage
        .upload_file(
            "lance/v7/shard_0/shard.bin",
            &file_path,
            ltsearch::indexing::UploadMode::CreateOnly,
        )
        .await
        .expect_err("expected CreateOnly upload over an existing object to fail");
    assert!(error
        .to_string()
        .contains("version artifacts are immutable"));
    assert_eq!(
        storage
            .read("lance/v7/shard_0/shard.bin")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"v1"
    );

    storage
        .upload_file(
            "lance/v7/shard_0/shard.bin",
            &file_path,
            ltsearch::indexing::UploadMode::Overwrite,
        )
        .await
        .expect("expected Overwrite upload to succeed");
}

#[tokio::test]
async fn publish_storage_compare_and_swap_updates_head_when_expected_matches() {
    let harness = MotoHarness::new("publish-storage-cas").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    let swapped = storage
        .compare_and_swap("index/_head", None, b"{}")
        .await
        .unwrap();
    assert!(swapped);
}

#[tokio::test]
async fn publish_storage_compare_and_swap_returns_false_when_expected_mismatches() {
    let harness = MotoHarness::new("publish-storage-cas-mismatch").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    assert!(storage
        .compare_and_swap("index/_head", None, b"old")
        .await
        .unwrap());
    assert!(!storage
        .compare_and_swap("index/_head", Some("\"bogus-etag\""), b"new")
        .await
        .unwrap());
    assert_eq!(
        storage.read("index/_head").await.unwrap().unwrap().bytes,
        b"old"
    );
}

#[tokio::test]
async fn publish_storage_compare_and_swap_rejects_stale_etag_and_existing_object() {
    let harness = MotoHarness::new("publish-storage-cas-stale").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    // Creation guarded by If-None-Match: second create must lose.
    assert!(storage
        .compare_and_swap("index/_head", None, b"v1")
        .await
        .unwrap());
    assert!(!storage
        .compare_and_swap("index/_head", None, b"v1-again")
        .await
        .unwrap());

    // Replacement guarded by If-Match: a stale ETag must lose.
    let first_etag = storage.read("index/_head").await.unwrap().unwrap().etag;
    assert!(storage
        .compare_and_swap("index/_head", Some(&first_etag), b"v2")
        .await
        .unwrap());
    assert!(!storage
        .compare_and_swap("index/_head", Some(&first_etag), b"v3")
        .await
        .unwrap());
    assert_eq!(
        storage.read("index/_head").await.unwrap().unwrap().bytes,
        b"v2"
    );
}

/// The AWS twin of the local `static-activate` flow (bin `static_activate`):
/// verify a built v3 release → immutably upload it under
/// `static/releases/<release_id>/` → CAS-flip `static/_head`. Asserts the stored
/// pointer resolves to the release and the manifest bytes landed verbatim in S3.
#[tokio::test]
async fn aws_static_activate_uploads_release_and_flips_pointer() {
    let harness = MotoHarness::new("static-activate").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();

    storage
        .upload_directory(
            &static_release_dir_key(&manifest.release_id),
            &dir,
            UploadMode::CreateOnly,
        )
        .await
        .unwrap();

    let result = activate_static_pointer(&storage, &manifest.release_id, 1_700_000_000_000)
        .await
        .unwrap();
    assert_eq!(result.previous_release_id, None);

    // The pointer object resolves back to the just-activated release.
    let head_object = storage
        .read(STATIC_HEAD_KEY)
        .await
        .unwrap()
        .expect("static/_head must exist after activation");
    let head = StaticReleaseHead::from_json(&head_object.bytes).unwrap();
    assert_eq!(head.release_id, manifest.release_id);

    // The release manifest landed in S3 byte-for-byte with the source on disk.
    let manifest_object = storage
        .read(&static_release_manifest_key(&manifest.release_id))
        .await
        .unwrap()
        .expect("release manifest must exist in S3");
    let on_disk = std::fs::read(dir.join("release_manifest.json")).unwrap();
    assert_eq!(manifest_object.bytes, on_disk);
}

/// Query-side cold start (Task 12): once a static release is activated remotely,
/// the first `S3ArtifactSync::sync` must land both the `static/_head` pointer and
/// the pointed-at release directory under `<root>/static/releases/<id>/` on local
/// disk. To make cache-hit vs erroneous-re-pull *observably distinct* (a deleted
/// remote would let a list-based re-pull silently succeed on an empty set), the
/// remote manifest is then overwritten with sentinel bytes: a second sync must
/// still be `Ok(())` and the local manifest must NOT contain the sentinel —
/// proving the release pull is gated on the local cache. Finally, deleting the
/// local release directory and syncing a third time must re-pull (the local
/// manifest now IS the sentinel), pinning the other half of the lazy predicate:
/// absent locally ⇒ pull.
#[tokio::test]
async fn s3_sync_pulls_pointer_and_active_release_once() {
    use ltsearch::adapters::s3_artifact_sync::S3ArtifactSync;
    use ltsearch::contracts::ArtifactSync;

    let harness = MotoHarness::new("s3-sync-pointer").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    // Seed the remote: immutable release upload + pointer activation.
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();
    storage
        .upload_directory(
            &static_release_dir_key(&manifest.release_id),
            &dir,
            UploadMode::CreateOnly,
        )
        .await
        .unwrap();
    activate_static_pointer(&storage, &manifest.release_id, 1_700_000_000_000)
        .await
        .unwrap();

    let artifact_root = harness.new_artifact_root();

    // First sync: pointer + release land on local disk.
    S3ArtifactSync::with_client(harness.bucket.clone(), harness.s3.clone())
        .sync(&artifact_root)
        .await
        .unwrap();

    let local_head = artifact_root.join(STATIC_HEAD_KEY);
    let local_manifest = artifact_root.join(static_release_manifest_key(&manifest.release_id));
    assert!(
        local_head.exists(),
        "static/_head pointer must land locally after first sync"
    );
    assert!(
        local_manifest.exists(),
        "active release manifest must land locally after first sync"
    );

    // Overwrite the remote manifest with sentinel bytes (raw put_object —
    // CreateOnly immutability is an AwsPublishStorage upload-layer semantic,
    // not an S3-enforced one). A correct implementation never reads it back; a
    // buggy always-re-pull implementation would copy the sentinel to local disk.
    const SENTINEL: &[u8] = b"TAMPERED";
    harness
        .s3
        .put_object()
        .bucket(&harness.bucket)
        .key(static_release_manifest_key(&manifest.release_id))
        .body(aws_sdk_s3::primitives::ByteStream::from_static(SENTINEL))
        .send()
        .await
        .unwrap();

    // Second sync: cache hit on the already-present release_id ⇒ no re-pull.
    S3ArtifactSync::with_client(harness.bucket.clone(), harness.s3.clone())
        .sync(&artifact_root)
        .await
        .unwrap();
    let after_second = std::fs::read(&local_manifest).unwrap();
    assert_ne!(
        after_second, SENTINEL,
        "second sync must hit the local cache and never re-pull the (tampered) remote manifest"
    );

    // Third sync after wiping the local release dir: the lazy predicate's other
    // half — absent locally ⇒ pull. The sentinel landing on disk proves the
    // re-pull actually happened.
    std::fs::remove_dir_all(artifact_root.join(static_release_dir_key(&manifest.release_id)))
        .unwrap();
    S3ArtifactSync::with_client(harness.bucket.clone(), harness.s3.clone())
        .sync(&artifact_root)
        .await
        .unwrap();
    let after_third = std::fs::read(&local_manifest).unwrap();
    assert_eq!(
        after_third, SENTINEL,
        "third sync must re-pull the release once it is absent locally"
    );
}

/// Crash-safety guard for the query-side release pull: the download must go
/// through a per-attempt-unique `.<id>-staging-*` directory that is atomically
/// renamed into the final `static/releases/<id>/` location (mirroring
/// `install_into_managed_store`). A staging dir left by a *different* attempt
/// (crashed process or concurrent pull) is foreign territory: the pull must
/// never wipe or reuse it — a shared deterministic staging path with an entry
/// wipe would let two concurrent pulls delete each other's half-downloaded
/// files and promote an incomplete final dir. This test seeds a foreign
/// staging dir and asserts three things after a sync: the final dir is
/// complete (manifest AND every `.bin`), the foreign staging dir survives
/// byte-for-byte untouched, and the pull leaked no staging dir of its own.
#[tokio::test]
async fn s3_sync_stages_release_pull_and_recovers_from_dirty_staging() {
    use ltsearch::adapters::s3_artifact_sync::S3ArtifactSync;
    use ltsearch::contracts::ArtifactSync;

    let harness = MotoHarness::new("s3-sync-staging").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();
    storage
        .upload_directory(
            &static_release_dir_key(&manifest.release_id),
            &dir,
            UploadMode::CreateOnly,
        )
        .await
        .unwrap();
    activate_static_pointer(&storage, &manifest.release_id, 1_700_000_000_000)
        .await
        .unwrap();

    // Simulate residue from a crashed (or concurrent) foreign pull attempt:
    // a dirty staging dir the sync must treat as untouchable. No final dir.
    let artifact_root = harness.new_artifact_root();
    let releases_parent = artifact_root.join("static/releases");
    let foreign_staging = releases_parent.join(format!(".{}-staging-junk", manifest.release_id));
    std::fs::create_dir_all(&foreign_staging).unwrap();
    std::fs::write(foreign_staging.join("junk-from-crashed-pull"), b"partial").unwrap();

    S3ArtifactSync::with_client(harness.bucket.clone(), harness.s3.clone())
        .sync(&artifact_root)
        .await
        .unwrap();

    // The final dir is complete: manifest plus every v3 artifact file.
    let final_dir = artifact_root.join(static_release_dir_key(&manifest.release_id));
    for name in V3_RELEASE_OUTPUT_FILES {
        assert!(
            final_dir.join(name).exists(),
            "final release dir must contain {name} after a staged pull"
        );
    }
    // The foreign staging dir survives untouched: no pull may wipe another
    // attempt's staging territory.
    assert_eq!(
        std::fs::read(foreign_staging.join("junk-from-crashed-pull")).unwrap(),
        b"partial",
        "foreign staging residue must be left exactly as found"
    );
    // And this pull converged: besides the foreign dir, no `.<id>-staging-*`
    // residue of its own was leaked.
    let staging_prefix = format!(".{}-staging", manifest.release_id);
    let leftover_stagings: Vec<String> = std::fs::read_dir(&releases_parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&staging_prefix))
        .collect();
    assert_eq!(
        leftover_stagings,
        vec![format!("{staging_prefix}-junk")],
        "the pull must leave exactly the foreign staging dir behind, none of its own"
    );
}

/// Path-escape defense: an S3 key may legally contain `..`, so a listed key like
/// `index/../../../ltsearch.db` would, via `artifact_root.join(key)`, escape the
/// artifact root and clobber files outside it (the SQLite control plane included).
/// The shared download engine must refuse any key with a non-normal path
/// component *before* the layout mapper runs, and write nothing. This seeds such a
/// key under the batch-pulled `index/` prefix and asserts the sync errors out with
/// no escaped file on disk.
#[tokio::test]
async fn s3_sync_refuses_path_traversal_key_and_writes_nothing() {
    use ltsearch::adapters::s3_artifact_sync::S3ArtifactSync;
    use ltsearch::contracts::ArtifactSync;

    let harness = MotoHarness::new("s3-sync-traversal").await;

    // A malicious/buggy key that escapes the artifact root by three levels.
    // It still lexically starts with `index/`, so it is returned by the
    // prefix-filtered ListObjectsV2 the batch pull issues.
    let evil_key = "index/../../../ltsearch.db";
    let put = harness
        .s3
        .put_object()
        .bucket(&harness.bucket)
        .key(evil_key)
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"pwned"))
        .send()
        .await;

    // Record whatever key Moto/the SDK actually stored — some stacks normalize
    // `..` client- or server-side. If it did not survive verbatim, the escape
    // vector never materializes and the pure-function unit tests are the
    // authoritative guard; skip the on-disk assertion in that case.
    let stored_key = harness
        .s3
        .list_objects_v2()
        .bucket(&harness.bucket)
        .prefix("index/")
        .send()
        .await
        .unwrap()
        .contents()
        .iter()
        .filter_map(|o| o.key())
        .find(|k| k.contains(".."))
        .map(|k| k.to_string());

    let artifact_root = harness.new_artifact_root();
    // The escape target: `<root>/../../../ltsearch.db`, i.e. three levels above.
    let escape_target = artifact_root
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.join("ltsearch.db"));

    let result = S3ArtifactSync::with_client(harness.bucket.clone(), harness.s3.clone())
        .sync(&artifact_root)
        .await;

    if put.is_ok() && stored_key.as_deref() == Some(evil_key) {
        // The escape key survived into S3 verbatim — the defense must trip.
        let error = result.expect_err("sync must reject a key with a non-normal path component");
        assert!(
            error.contains("non-normal path component"),
            "unexpected error: {error}"
        );
        if let Some(target) = &escape_target {
            assert!(
                !target.exists(),
                "path-traversal key must not write outside the artifact root: {}",
                target.display()
            );
        }
        assert!(
            !artifact_root.join("ltsearch.db").exists(),
            "no escaped artifact may be written under the root either"
        );
    } else {
        eprintln!(
            "note: Moto/SDK did not store `{evil_key}` verbatim (stored: {stored_key:?}); \
             on-disk escape assertion skipped — pure-function unit tests remain authoritative"
        );
    }
}

/// Runs the real `static_activate` bin as a subprocess pointed at Moto, so the
/// tests exercise the bin's actual install orchestration end-to-end (not a
/// reproduction of it). Credentials + endpoint are injected via env to match the
/// `MotoHarness` client wiring; the S3 endpoint override is honoured by
/// `bootstrap::s3_client_from_env`.
fn run_static_activate_bin(bucket: &str, release_dir: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_static_activate"))
        .arg("--release")
        .arg(release_dir)
        .env("LTSEARCH_QUERY_S3_BUCKET", bucket)
        .env("AWS_ENDPOINT_URL_S3", "http://localhost:5000")
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .expect("failed to spawn static_activate bin")
}

/// Finding 2 end-to-end guard: running the bin twice against the same release
/// must succeed both times. The second run hits a CreateOnly conflict on every
/// object including the manifest; the bin's conflict matcher (pinned to the
/// adapter's `CREATE_ONLY_CONFLICT_PHRASE` const) must recognise it and report
/// "already fully installed" instead of turning the idempotent re-run fatal. A
/// wording drift between the adapter's message and the bin's matcher would break
/// this test.
#[tokio::test]
async fn aws_static_activate_bin_second_run_is_idempotent() {
    let harness = MotoHarness::new("static-activate-idempotent").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();

    let first = run_static_activate_bin(&harness.bucket, &dir);
    assert!(
        first.status.success(),
        "first install must succeed; stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run_static_activate_bin(&harness.bucket, &dir);
    assert!(
        second.status.success(),
        "second install must be idempotent, not fatal; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second_stderr.contains("already fully installed"),
        "second run should report the already-installed outcome; stderr: {second_stderr}"
    );

    // The pointer resolves to the release and the manifest landed byte-for-byte.
    let head_object = storage
        .read(STATIC_HEAD_KEY)
        .await
        .unwrap()
        .expect("static/_head must exist after activation");
    let head = StaticReleaseHead::from_json(&head_object.bytes).unwrap();
    assert_eq!(head.release_id, manifest.release_id);
    let manifest_object = storage
        .read(&static_release_manifest_key(&manifest.release_id))
        .await
        .unwrap()
        .expect("release manifest must exist in S3");
    let on_disk = std::fs::read(dir.join("release_manifest.json")).unwrap();
    assert_eq!(manifest_object.bytes, on_disk);
}

/// Finding 1 end-to-end guard: a prior run that died mid-upload leaves a partial
/// `.bin` set and NO manifest. The bin must resume — backfilling the remaining
/// objects and finally writing the manifest completeness marker — rather than
/// treating the first already-present object as "fully installed" and flipping
/// the pointer at an incomplete release. This test FAILS against the old
/// all-or-nothing `upload_directory` + first-conflict-means-installed logic,
/// which would skip the remaining objects and never upload the manifest.
#[tokio::test]
async fn aws_static_activate_bin_resumes_partial_prior_upload() {
    let harness = MotoHarness::new("static-activate-resume").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();
    let dir_key = static_release_dir_key(&manifest.release_id);

    // Simulate a dead prior run: place exactly ONE .bin object, no manifest.
    let partial = V3_RELEASE_OUTPUT_FILES[0];
    storage
        .upload_file(
            &format!("{dir_key}/{partial}"),
            &dir.join(partial),
            UploadMode::CreateOnly,
        )
        .await
        .unwrap();
    assert!(
        storage
            .read(&static_release_manifest_key(&manifest.release_id))
            .await
            .unwrap()
            .is_none(),
        "precondition: manifest must be absent before the resume run"
    );

    let out = run_static_activate_bin(&harness.bucket, &dir);
    assert!(
        out.status.success(),
        "resume install must complete; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every one of the nine .bin objects is now present (the pre-placed one plus
    // the eight backfilled by the resume run).
    for name in V3_RELEASE_OUTPUT_FILES {
        assert!(
            storage
                .read(&format!("{dir_key}/{name}"))
                .await
                .unwrap()
                .is_some(),
            "object {name} must be present after resume"
        );
    }
    // The manifest completeness marker landed byte-for-byte...
    let manifest_object = storage
        .read(&static_release_manifest_key(&manifest.release_id))
        .await
        .unwrap()
        .expect("manifest must land only after all .bin objects are present");
    let on_disk = std::fs::read(dir.join("release_manifest.json")).unwrap();
    assert_eq!(manifest_object.bytes, on_disk);
    // ...and only then did the pointer flip to the now-complete release.
    let head_object = storage
        .read(STATIC_HEAD_KEY)
        .await
        .unwrap()
        .expect("static/_head must exist after activation");
    let head = StaticReleaseHead::from_json(&head_object.bytes).unwrap();
    assert_eq!(head.release_id, manifest.release_id);
}

/// Finding 1 guard: a pre-existing object under the release prefix carrying
/// WRONG bytes must abort activation. `verify_release_dir` only proves the LOCAL
/// dir, so the bin must read each conflicting object back and verify its sha256 +
/// size against the verified manifest. A corrupt pre-planted `.bin` must be fatal
/// — the manifest completeness marker must NOT be uploaded and `static/_head`
/// must NOT flip. Without the read-back integrity check the bin would skip the
/// corrupt object, upload the rest + manifest, and activate a corrupt release.
#[tokio::test]
async fn aws_static_activate_bin_rejects_preexisting_corrupt_object() {
    let harness = MotoHarness::new("static-activate-corrupt").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();
    let dir_key = static_release_dir_key(&manifest.release_id);

    // Pre-plant ONE object under the release prefix with DIFFERENT bytes than the
    // release expects (a corrupt / colliding prior write).
    let corrupt_name = V3_RELEASE_OUTPUT_FILES[0];
    let corrupt_key = format!("{dir_key}/{corrupt_name}");
    let corrupt_path = std::env::temp_dir().join(format!(
        "ltsearch-corrupt-preplant-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&corrupt_path, b"totally-wrong-bytes-not-the-real-artifact").unwrap();
    storage
        .upload_file(&corrupt_key, &corrupt_path, UploadMode::CreateOnly)
        .await
        .unwrap();

    let out = run_static_activate_bin(&harness.bucket, &dir);
    assert!(
        !out.status.success(),
        "activation must abort on a corrupt pre-existing object; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&corrupt_key),
        "stderr must name the corrupt object {corrupt_key}; stderr: {stderr}"
    );

    // The pointer must NOT have flipped and the manifest marker must NOT exist.
    assert!(
        storage.read(STATIC_HEAD_KEY).await.unwrap().is_none(),
        "static/_head must not exist after a rejected corrupt activation"
    );
    assert!(
        storage
            .read(&static_release_manifest_key(&manifest.release_id))
            .await
            .unwrap()
            .is_none(),
        "manifest completeness marker must not be uploaded when activation aborts"
    );
}

#[tokio::test]
async fn publish_storage_read_propagates_non_missing_object_errors() {
    let server = MockS3Server::start(vec![MockHttpResponse::access_denied()]);
    let storage = AwsPublishStorage::new(
        "test-bucket",
        s3_client_for_endpoint(&server.endpoint_url).await,
    );

    let error = storage
        .read("index/_head")
        .await
        .expect_err("expected read to fail");

    assert!(error
        .to_string()
        .contains("failed to load object index/_head"));
    assert_eq!(
        server.finish(),
        vec!["GET /test-bucket/index/_head?x-id=GetObject".to_string()]
    );
}

#[tokio::test]
async fn s3_wal_append_stops_before_put_when_existing_read_fails() {
    let server = MockS3Server::start(vec![MockHttpResponse::access_denied()]);
    let wal = AwsS3WalStorage::new(
        "test-bucket",
        s3_client_for_endpoint(&server.endpoint_url).await,
    );

    let error = wal
        .append("wal/2023/11/14/batch-test.jsonl", b"line-2\n")
        .await
        .expect_err("expected append to fail");

    assert!(error
        .to_string()
        .contains("failed to load existing WAL object wal/2023/11/14/batch-test.jsonl"));
    assert_eq!(
        server.finish(),
        vec!["GET /test-bucket/wal/2023/11/14/batch-test.jsonl?x-id=GetObject".to_string()]
    );
}

#[tokio::test]
async fn moto_harness_receives_and_decodes_one_queue_batch() {
    let harness = MotoHarness::new("decode-batch").await;
    AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone())
        .enqueue(ltsearch::write::QueueBatch {
            batch_id: "batch-xyz".into(),
            wal_key: "wal/2023/11/14/batch-xyz.jsonl".into(),
            accepted_count: 1,
            wal_event_ids: vec!["batch-xyz-000001".into()],
        })
        .await
        .unwrap();

    let batch = harness.receive_batch().await;
    assert_eq!(batch.batch_id, "batch-xyz");
    assert_eq!(batch.accepted_count, 1);
}

#[tokio::test]
async fn write_build_publish_flow_runs_end_to_end_against_moto() {
    let harness = MotoHarness::new("write-build-publish").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let api = ltsearch::write::WriteApi::new(wal, queue).with_clock(|| 1_700_000_000_000);

    let response = api.ingest(vec![sample_document("doc-1")]).await.unwrap();
    harness.assert_wal_object_exists(&response.batch_id).await;

    let batch = harness.receive_batch().await;
    let build_result = harness.consume_build_and_publish(batch).await;

    assert_eq!(build_result.manifest.version_id, 1);
    harness.assert_manifest_exists(1).await;
    harness.assert_head_points_to(1).await;
}

#[tokio::test]
async fn publish_step_uses_original_build_artifacts_instead_of_rebuilding_documents() {
    let harness = MotoHarness::new("publish-original-build").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let api = ltsearch::write::WriteApi::new(wal, queue).with_clock(|| 1_700_000_000_000);

    let response = api.ingest(vec![sample_document("doc-1")]).await.unwrap();
    let batch = harness.receive_batch().await;
    let build_request = harness.build_request_from_batch(batch).await;
    let mut build_result = harness.build_from_batch(&build_request);
    let original_document_count = build_result.manifest.document_count;

    assert_eq!(response.accepted_count, original_document_count);

    let artifact_root = harness.latest_build_artifact_root();
    build_result.documents.clear();
    harness
        .publish_build_result(&build_result, artifact_root)
        .await;

    let manifest = harness.read_manifest(1).await;
    assert_eq!(manifest.document_count, original_document_count);
}

impl MotoHarness {
    async fn new(name: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let bucket = format!("ltsearch-{name}-{suffix}").to_lowercase();
        let queue_name = format!("ltsearch-{name}-{suffix}");
        let artifact_root =
            std::env::temp_dir().join(format!("ltsearch-build-publish-artifacts-{name}-{suffix}"));

        let credentials = Credentials::new("test", "test", None, None, "moto");
        let region = Region::new("us-east-1");

        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(region.clone())
            .credentials_provider(credentials)
            .endpoint_url("http://localhost:5000")
            .load()
            .await;

        let s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
            .force_path_style(true)
            .build();
        let s3 = S3Client::from_conf(s3_config);
        let sqs = SqsClient::new(&shared_config);

        let queue_url = wait_until_ready(&s3, &sqs, &bucket, &queue_name).await;

        Self {
            artifact_root,
            bucket,
            queue_url,
            s3,
            sqs,
        }
    }

    async fn bucket_exists(&self) -> bool {
        self.s3
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
    }

    async fn queue_exists(&self) -> bool {
        self.sqs
            .get_queue_attributes()
            .queue_url(&self.queue_url)
            .attribute_names(aws_sdk_sqs::types::QueueAttributeName::QueueArn)
            .send()
            .await
            .is_ok()
    }

    async fn read_s3_text(&self, key: &str) -> String {
        let object = self
            .s3
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .unwrap();
        let bytes = object.body.collect().await.unwrap().into_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn receive_one_message_body(&self) -> String {
        let response = self
            .sqs
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(5)
            .send()
            .await
            .unwrap();

        response
            .messages
            .unwrap_or_default()
            .into_iter()
            .next()
            .and_then(|message| message.body)
            .expect("expected one queue message")
    }

    async fn receive_batch(&self) -> ltsearch::write::QueueBatch {
        let response = self
            .sqs
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(5)
            .send()
            .await
            .unwrap();

        let message = response
            .messages
            .unwrap_or_default()
            .into_iter()
            .next()
            .expect("expected one queue message");

        let receipt_handle = message
            .receipt_handle
            .clone()
            .expect("missing receipt handle");
        let body = message.body.clone().expect("missing message body");
        let batch = serde_json::from_str(&body).unwrap();

        self.sqs
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt_handle)
            .send()
            .await
            .unwrap();

        batch
    }

    fn new_artifact_root(&self) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = std::env::temp_dir().join(format!("ltsearch-artifacts-{suffix}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    async fn assert_wal_object_exists(&self, batch_id: &str) {
        let key = format!("wal/2023/11/14/{batch_id}.jsonl");
        let _ = self.read_s3_text(&key).await;
    }

    async fn consume_build_and_publish(
        &self,
        batch: ltsearch::write::QueueBatch,
    ) -> ltsearch::indexing::BuildIndexResult {
        let build_request = self.build_request_from_batch(batch).await;
        let build_result = self.build_from_batch(&build_request);
        self.publish_build_result(&build_result, self.latest_build_artifact_root())
            .await;
        build_result
    }

    fn latest_build_artifact_root(&self) -> std::path::PathBuf {
        self.artifact_root.clone()
    }

    async fn assert_manifest_exists(&self, version_id: u64) {
        let key = format!("index/versions/{version_id}/manifest.json");
        let object = AwsPublishStorage::new(self.bucket.clone(), self.s3.clone())
            .read(&key)
            .await
            .unwrap();
        assert!(object.is_some(), "missing manifest object at {key}");
    }

    async fn read_manifest(&self, version_id: u64) -> ltsearch::models::IndexManifest {
        let key = format!("index/versions/{version_id}/manifest.json");
        let object = AwsPublishStorage::new(self.bucket.clone(), self.s3.clone())
            .read(&key)
            .await
            .unwrap()
            .expect("missing manifest object");
        serde_json::from_slice(&object.bytes).unwrap()
    }

    async fn assert_head_points_to(&self, version_id: u64) {
        let object = AwsPublishStorage::new(self.bucket.clone(), self.s3.clone())
            .read(ltsearch::storage::INDEX_HEAD_KEY)
            .await
            .unwrap()
            .expect("missing _head object");
        let head: ltsearch::storage::ManifestHead = serde_json::from_slice(&object.bytes).unwrap();
        assert_eq!(head.version_id, version_id);
        assert_eq!(
            head.manifest_path,
            format!("index/versions/{version_id}/manifest.json")
        );
    }

    async fn build_request_from_batch(
        &self,
        batch: ltsearch::write::QueueBatch,
    ) -> BuildIndexRequest {
        let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
            self.bucket.clone(),
            self.s3.clone(),
        ));
        let records = wal.read(&batch.wal_key).await.unwrap();

        BuildIndexRequest {
            version_id: 1,
            created_at: 1_700_000_000_500,
            embedding_dim: 1,
            records,
        }
    }

    fn build_from_batch(&self, request: &BuildIndexRequest) -> BuildIndexResult {
        let artifact_root = self.latest_build_artifact_root();
        let _ = std::fs::remove_dir_all(&artifact_root);
        std::fs::create_dir_all(&artifact_root).unwrap();
        let builder = LocalIndexBuilder::new(&artifact_root, FixedEmbeddingGenerator);
        builder.build(request).unwrap()
    }

    async fn publish_build_result(
        &self,
        build_result: &BuildIndexResult,
        artifact_root: std::path::PathBuf,
    ) {
        let publisher = IndexPublisher::new(
            &artifact_root,
            AwsPublishStorage::new(self.bucket.clone(), self.s3.clone()),
        );
        publisher
            .publish(&PublishRequest {
                manifest: build_result.manifest.clone(),
                expected_current_version: None,
                updated_at: 1_700_000_000_900,
            })
            .await
            .unwrap();
    }
}

async fn wait_until_ready(
    s3: &S3Client,
    sqs: &SqsClient,
    bucket: &str,
    queue_name: &str,
) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut bucket_ready = false;
    let mut queue_url = None;
    let mut last_error = String::new();

    loop {
        if !bucket_ready {
            match s3.create_bucket().bucket(bucket).send().await {
                Ok(_) => bucket_ready = true,
                Err(error) => {
                    if s3.head_bucket().bucket(bucket).send().await.is_ok() {
                        bucket_ready = true;
                    } else {
                        last_error = format!("bucket={error:?}");
                    }
                }
            }
        }

        if queue_url.is_none() {
            match sqs.create_queue().queue_name(queue_name).send().await {
                Ok(queue) => queue_url = queue.queue_url,
                Err(error) => match sqs.get_queue_url().queue_name(queue_name).send().await {
                    Ok(existing) => queue_url = existing.queue_url,
                    Err(_) => {
                        last_error = format!("queue={error:?}");
                    }
                },
            }
        }

        if let (true, Some(queue_url)) = (bucket_ready, queue_url.clone()) {
            return queue_url;
        }

        if std::time::Instant::now() >= deadline {
            panic!("Moto did not become ready: {last_error}");
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn test_write_api() -> ltsearch::write::WriteApi<TestWalStorage, TestBuildQueue> {
    let wal = ltsearch::write::WriteAheadLog::new(TestWalStorage);
    let queue = TestBuildQueue;
    ltsearch::write::WriteApi::new(wal, queue)
}

fn test_publisher() -> ltsearch::indexing::IndexPublisher<TestPublishStorage> {
    let artifact_root = std::env::temp_dir().join("ltsearch-publisher-smoke");
    let request = test_publish_request();
    std::fs::create_dir_all(artifact_root.join("index/versions/1")).unwrap();
    std::fs::create_dir_all(artifact_root.join("index/versions/1/shards/0.lance")).unwrap();
    std::fs::create_dir_all(artifact_root.join("index/versions/1/shards/0.tantivy")).unwrap();
    std::fs::write(
        artifact_root.join("index/versions/1/shards/0.lance/data.bin"),
        b"lance",
    )
    .unwrap();
    std::fs::write(
        artifact_root.join("index/versions/1/shards/0.tantivy/meta.json"),
        b"tantivy",
    )
    .unwrap();
    std::fs::write(
        artifact_root.join("index/versions/1/manifest.json"),
        serde_json::to_vec_pretty(&request.manifest).unwrap(),
    )
    .unwrap();
    ltsearch::indexing::IndexPublisher::new(artifact_root, TestPublishStorage)
}

fn test_publish_request() -> ltsearch::indexing::PublishRequest {
    ltsearch::indexing::PublishRequest {
        manifest: ltsearch::models::IndexManifest {
            version_id: 1,
            created_at: 1_700_000_000_000,
            embedding_dim: 1,
            document_count: 0,
            num_shards: 1,
            shards: vec![ltsearch::models::ShardManifest {
                shard_id: 0,
                document_count: 0,
                lance_path: "s3://bucket/index/versions/1/shards/0.lance".into(),
                tantivy_path: "s3://bucket/index/versions/1/shards/0.tantivy".into(),
            }],
        },
        expected_current_version: None,
        updated_at: 1_700_000_000_100,
    }
}

#[derive(Clone)]
struct TestWalStorage;

#[async_trait]
impl ltsearch::write::WalStorage for TestWalStorage {
    async fn append(&self, _key: &str, _bytes: &[u8]) -> Result<(), ltsearch::error::IngestError> {
        Ok(())
    }

    async fn read(&self, _key: &str) -> Result<Vec<u8>, ltsearch::error::IngestError> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
struct TestBuildQueue;

#[async_trait]
impl ltsearch::write::BuildQueue for TestBuildQueue {
    async fn enqueue(
        &self,
        _batch: ltsearch::write::QueueBatch,
    ) -> Result<(), ltsearch::error::IngestError> {
        Ok(())
    }
}

struct FixedEmbeddingGenerator;

impl EmbeddingGenerator for FixedEmbeddingGenerator {
    fn generate(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![1.0])
    }
}

fn sample_document(doc_id: &str) -> ltsearch::models::Document {
    ltsearch::models::Document {
        doc_id: doc_id.into(),
        text: format!("document {doc_id}"),
        embedding: None,
        metadata: std::collections::HashMap::new(),
        timestamp: 1_700_000_000_000,
    }
}

struct MockS3Server {
    endpoint_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockS3Server {
    fn start(responses: Vec<MockHttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&requests);

        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request_line = read_http_request_line(&mut stream);
                recorded_requests.lock().unwrap().push(request_line);
                stream.write_all(&response.to_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        Self {
            endpoint_url: format!("http://{address}"),
            requests,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Vec<String> {
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

struct MockHttpResponse {
    status_line: &'static str,
    body: &'static str,
}

impl MockHttpResponse {
    fn access_denied() -> Self {
        Self {
            status_line: "HTTP/1.1 403 Forbidden",
            body: "<Error><Code>AccessDenied</Code><Message>denied</Message></Error>",
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        format!(
            "{}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/xml\r\n\r\n{}",
            self.status_line,
            self.body.len(),
            self.body
        )
        .into_bytes()
    }
}

fn read_http_request_line(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = [0_u8; 4096];
    let mut request = Vec::new();

    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let first_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default()
        .strip_suffix(b"\r")
        .unwrap_or_default();
    let request_line = String::from_utf8(first_line.to_vec()).unwrap();
    let mut parts = request_line.split_whitespace();

    format!(
        "{} {}",
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default()
    )
}

async fn s3_client_for_endpoint(endpoint_url: &str) -> S3Client {
    let credentials = Credentials::new("test", "test", None, None, "mock-s3");
    let region = Region::new("us-east-1");
    let shared_config = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .credentials_provider(credentials)
        .retry_config(RetryConfig::disabled())
        .endpoint_url(endpoint_url)
        .load()
        .await;

    let s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
        .force_path_style(true)
        .build();
    S3Client::from_conf(s3_config)
}

#[derive(Clone)]
struct TestPublishStorage;

#[async_trait]
impl ltsearch::indexing::PublishStorage for TestPublishStorage {
    async fn upload_directory(
        &self,
        _key: &str,
        _source: &std::path::Path,
        _mode: ltsearch::indexing::UploadMode,
    ) -> Result<(), ltsearch::error::PublishError> {
        Ok(())
    }

    async fn upload_file(
        &self,
        _key: &str,
        _source: &std::path::Path,
        _mode: ltsearch::indexing::UploadMode,
    ) -> Result<(), ltsearch::error::PublishError> {
        Ok(())
    }

    async fn read(
        &self,
        _key: &str,
    ) -> Result<Option<ltsearch::indexing::VersionedObject>, ltsearch::error::PublishError> {
        Ok(None)
    }

    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected_etag: Option<&str>,
        _new_value: &[u8],
    ) -> Result<bool, ltsearch::error::PublishError> {
        Ok(true)
    }
}
