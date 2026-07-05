use std::sync::atomic::{AtomicBool, Ordering};

use ltsearch::build_lambda::{handle_build_request, BuildRequest};
use ltsearch::error::{IndexError, PublishError};
use ltsearch::indexing::{BuildIndexResult, PublishResult};
use ltsearch::models::{IndexManifest, ShardManifest};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn sample_build_request() -> BuildRequest {
    BuildRequest {
        batch_id: "batch-abc".into(),
        wal_key: "wal/2026/03/19/batch-abc.jsonl".into(),
        version_id: 1,
        embedding_dim: 3,
    }
}

fn sample_manifest() -> IndexManifest {
    IndexManifest {
        version_id: 1,
        created_at: 1700000000000,
        embedding_dim: 3,
        document_count: 1,
        num_shards: 1,
        shards: vec![ShardManifest {
            shard_id: 0,
            document_count: 1,
            lance_path: "s3://bucket/lance/v1/shard_0".into(),
            tantivy_path: "s3://bucket/index/v1/shard_0".into(),
        }],
    }
}

#[test]
fn build_lambda_returns_success_on_successful_build_and_publish() {
    let result = block_on(handle_build_request(
        async |_request| {
            Ok(BuildIndexResult {
                manifest: sample_manifest(),
                documents: vec![],
            })
        },
        async |_manifest| {
            Ok(PublishResult {
                activated_version_id: 1,
                previous_version_id: None,
            })
        },
        sample_build_request(),
    ));

    let response = result.unwrap();
    assert_eq!(response.activated_version_id, 1);
    assert_eq!(response.previous_version_id, None);
    assert_eq!(response.document_count, 0);
}

#[test]
fn build_lambda_maps_build_failure_to_error_envelope() {
    let result = block_on(handle_build_request(
        async |_request| {
            Err(IndexError::Operation {
                message: "disk full".into(),
            })
        },
        async |_manifest| panic!("publish should not be called when build fails"),
        sample_build_request(),
    ));

    let error = result.unwrap_err();
    assert_eq!(error.error_type, "build_error");
    assert_eq!(error.message, "index operation failed: disk full");
}

#[test]
fn build_lambda_maps_publish_failure_to_error_envelope() {
    let result = block_on(handle_build_request(
        async |_request| {
            Ok(BuildIndexResult {
                manifest: sample_manifest(),
                documents: vec![],
            })
        },
        async |_manifest| {
            Err(PublishError::Operation {
                message: "CAS conflict".into(),
            })
        },
        sample_build_request(),
    ));

    let error = result.unwrap_err();
    assert_eq!(error.error_type, "publish_error");
    assert_eq!(error.message, "publish operation failed: CAS conflict");
}

#[test]
fn build_lambda_does_not_publish_when_build_fails() {
    let publish_called = AtomicBool::new(false);

    let result = block_on(handle_build_request(
        async |_request| {
            Err(IndexError::Operation {
                message: "build failed".into(),
            })
        },
        async |_manifest| {
            publish_called.store(true, Ordering::SeqCst);
            Ok(PublishResult {
                activated_version_id: 1,
                previous_version_id: None,
            })
        },
        sample_build_request(),
    ));

    assert!(result.is_err());
    assert!(
        !publish_called.load(Ordering::SeqCst),
        "publish handler should not be called when build fails"
    );
}
