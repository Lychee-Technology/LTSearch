//! End-to-end coverage for the static release activation orchestration:
//! `verify_release_dir` (8-step self-consistency), `install_into_managed_store`
//! (idempotent local install), and `activate_static_pointer` (CAS the pointer).

use std::fs;

use ltsearch::index::{derive_release_id, ReleaseManifest};
use ltsearch::indexing::{
    activate_static_pointer, install_into_managed_store, verify_release_dir, PublishStorage,
    StaticActivateError,
};
use ltsearch::storage::{StaticReleaseHead, STATIC_HEAD_KEY};

mod support;
use support::{build_v3_release_fixture, corrupt_one_byte, RecordingPublishStorage};

// --- verify_release_dir ------------------------------------------------------

#[test]
fn verify_rejects_tampered_output_hash() {
    let dir = build_v3_release_fixture();
    corrupt_one_byte(&dir.join("turbo_static_text.bin"));
    assert!(matches!(
        verify_release_dir(&dir, None, None).unwrap_err(),
        StaticActivateError::Verify { .. }
    ));
}

#[test]
fn verify_rejects_unexpected_model_id() {
    let dir = build_v3_release_fixture();
    assert!(matches!(
        verify_release_dir(&dir, Some("wrong-model"), None).unwrap_err(),
        StaticActivateError::Verify { .. }
    ));
}

#[test]
fn verify_rejects_manifest_with_missing_output_entry() {
    // A crafted manifest that drops one of the nine v3 `.bin` outputs, with
    // `release_id` re-derived over the *reduced* output set so the forged
    // manifest stays self-consistent through steps 1-4. The file still exists on
    // disk (MmapIndex would still read it by fixed name), so only an explicit
    // "outputs must cover all nine artifacts" check can catch this.
    let dir = build_v3_release_fixture();
    let manifest_path = dir.join("release_manifest.json");
    let mut manifest: ReleaseManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();

    // Drop one listed output (its file is left on disk untouched).
    let dropped = manifest
        .outputs
        .iter()
        .position(|output| output.name == "turbo_static_title.bin")
        .expect("fixture must list turbo_static_title.bin");
    manifest.outputs.remove(dropped);

    // Re-derive release_id over the reduced set so steps 1-4 all pass.
    manifest.release_id = derive_release_id(
        manifest.turbo_version,
        &manifest.embedding_profile,
        &manifest.codec,
        &manifest.input_fingerprint.content_digest,
        &manifest.outputs,
    );
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    assert!(matches!(
        verify_release_dir(&dir, None, None).unwrap_err(),
        StaticActivateError::Verify { .. }
    ));
}

#[test]
fn verify_rejects_reordered_manifest_outputs() {
    // A crafted manifest that lists the nine v3 outputs as the SAME set but in a
    // non-ascending order. Because `derive_release_id` sorts outputs internally,
    // the release_id is unaffected by the reorder — we assert that premise by NOT
    // re-deriving it after swapping. The `ReleaseManifest` contract requires
    // outputs stored name-ascending, so verify must reject on the order alone.
    let dir = build_v3_release_fixture();
    let manifest_path = dir.join("release_manifest.json");
    let original: ReleaseManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();

    let mut manifest = original.clone();
    manifest.outputs.swap(0, 1);
    // Premise: swapping two outputs does not change the content-derived id.
    let rederived = derive_release_id(
        manifest.turbo_version,
        &manifest.embedding_profile,
        &manifest.codec,
        &manifest.input_fingerprint.content_digest,
        &manifest.outputs,
    );
    assert_eq!(
        rederived, original.release_id,
        "premise: derive_release_id sorts internally, so a reorder is self-consistent"
    );
    // Leave release_id untouched (it already equals the derived id).
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let error = verify_release_dir(&dir, None, None).unwrap_err();
    match error {
        StaticActivateError::Verify { message } => assert!(
            message.contains("not name-ascending"),
            "expected an order-violation message, got: {message}"
        ),
        other => panic!("expected Verify error, got {other:?}"),
    }
}

#[test]
fn verify_accepts_valid_release() {
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, Some(512)).unwrap();
    assert_eq!(manifest.turbo_version, 3);
    assert_eq!(manifest.release_id.len(), 64);
}

// --- install_into_managed_store ----------------------------------------------

#[test]
fn install_into_managed_store_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let src = build_v3_release_fixture();
    let rid = "c".repeat(64);
    install_into_managed_store(root.path(), &rid, &src).unwrap();
    install_into_managed_store(root.path(), &rid, &src).unwrap(); // 二次不报错
    assert!(root
        .path()
        .join(format!("static/releases/{rid}/release_manifest.json"))
        .exists());
}

// --- activate_static_pointer -------------------------------------------------

#[tokio::test]
async fn activate_writes_pointer_when_none_present() {
    let storage = RecordingPublishStorage::default();
    let res = activate_static_pointer(&storage, &"a".repeat(64), 1_700_000_000_000)
        .await
        .unwrap();
    assert_eq!(res.previous_release_id, None);
    let obj = storage.read(STATIC_HEAD_KEY).await.unwrap().unwrap();
    let head = StaticReleaseHead::from_json(&obj.bytes).unwrap();
    assert_eq!(head.release_id, "a".repeat(64));
}

#[tokio::test]
async fn activate_reports_lost_cas_on_conflict() {
    let storage = RecordingPublishStorage::default();
    // 预植抢先写入的现值 → 我方 expected(None) 过期 → lost CAS
    storage.conflict_on_compare_and_swap(
        StaticReleaseHead::new("f".repeat(64), 1_700_000_000_000)
            .to_json_pretty()
            .into_bytes(),
    );
    let err = activate_static_pointer(&storage, &"b".repeat(64), 1_700_000_000_001)
        .await
        .unwrap_err();
    assert!(matches!(err, StaticActivateError::LostCas { .. }));
}
