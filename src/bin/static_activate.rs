//! `static_activate` — the AWS twin of the local `ltsearch static-activate` CLI.
//!
//! Verifies a built v3 or v4 release directory, uploads it immutably to S3
//! under `static/releases/<release_id>/` (CreateOnly / `If-None-Match`), then
//! compare-and-swaps the `static/_head` pointer to it. The three steps are
//! strictly ordered: verify precedes upload, upload precedes the pointer swap.
//!
//! Idempotent *and* resumable: because `release_id` is content-derived,
//! re-running the same release re-targets the same immutable keys. The `.bin`
//! objects of the manifest's `turbo_version` (`release_output_files`) are
//! uploaded one at a time — a CreateOnly conflict on any one
//! means a prior run already placed it, so it is skipped and the rest resume.
//! `release_manifest.json` is uploaded strictly last as the completeness
//! marker: only a fully-installed release has a manifest object, so a manifest
//! conflict (byte-identical to the local one) is the single signal that means
//! "already completely installed", and only then do we CAS the pointer. A run
//! that died mid-upload leaves an incomplete `.bin` set but no manifest, so the
//! retry backfills rather than activating an incomplete release. A manifest
//! conflict with *different* bytes is treated as corruption (content-addressed
//! ids make it impossible). Every other upload error is fatal.
//!
//! Contract:
//! ```text
//! static_activate --release <built_release_dir> [--expect-model-id <id>] [--expect-dim <n>]
//! env: LTSEARCH_QUERY_S3_BUCKET (required) — the bucket the query stack reads,
//!      and therefore the exact bucket activation must write to.
//! ```

use std::env;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::adapters::s3_publish::{AwsPublishStorage, CREATE_ONLY_CONFLICT_PHRASE};
use ltsearch::bootstrap::s3_client_from_env;
use ltsearch::error::PublishError;
use ltsearch::index::{release_output_files, sha256_hex, ReleaseManifest, RELEASE_MANIFEST_FILE};
use ltsearch::indexing::{activate_static_pointer, verify_release_dir, PublishStorage, UploadMode};
use ltsearch::storage::{static_release_dir_key, static_release_manifest_key};

/// The bucket the query stack reads (`query_service.rs` →
/// `LTSEARCH_QUERY_S3_BUCKET`), and therefore the exact bucket activation must
/// write to. Single source of truth for the activation target.
const QUERY_BUCKET_ENV: &str = "LTSEARCH_QUERY_S3_BUCKET";
/// Deprecated bucket var this branch introduced. It is no longer a bucket
/// *source*; it is retained only as a transition guard against stale runbooks
/// (see [`resolve_bucket`]).
const LEGACY_BUCKET_ENV: &str = "LTSEARCH_STATIC_S3_BUCKET";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    release_dir: String,
    expect_model_id: Option<String>,
    expect_dim: Option<u32>,
}

/// Hand-rolled `--release [--expect-model-id --expect-dim]` parser (mirrors the
/// style of the other bins and `app.rs`, no clap). Skips `argv[0]`.
fn parse_args<I, S>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut release_dir = None;
    let mut expect_model_id = None;
    let mut expect_dim = None;
    let mut iter = args.into_iter();
    let _binary = iter.next();

    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "--release" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --release".to_string())?;
                release_dir = Some(value.as_ref().to_string());
            }
            "--expect-model-id" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --expect-model-id".to_string())?;
                expect_model_id = Some(value.as_ref().to_string());
            }
            "--expect-dim" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --expect-dim".to_string())?;
                let dim = value.as_ref().parse::<u32>().map_err(|_| {
                    format!(
                        "--expect-dim must be a positive integer, got {}",
                        value.as_ref()
                    )
                })?;
                expect_dim = Some(dim);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(CliArgs {
        release_dir: release_dir.ok_or_else(|| "missing required --release".to_string())?,
        expect_model_id,
        expect_dim,
    })
}

/// Resolves the activation target bucket, converging on the bucket the query
/// stack reads (`LTSEARCH_QUERY_S3_BUCKET`). Activation must write exactly where
/// the query side reads, so that env var is the single source of truth.
///
/// The deprecated `LTSEARCH_STATIC_S3_BUCKET` (introduced by this branch, no
/// external consumers) is no longer a bucket source. Transition guard: if it is
/// still set AND names a *different* bucket than the query var, hard-error naming
/// both values so a stale runbook cannot silently publish somewhere the query
/// side never reads. If it is set and equal (or unset), proceed.
///
/// Pure over its inputs (no `env::var` calls) so it is unit-testable without the
/// process-global environment and its attendant test races.
fn resolve_bucket(query: Option<String>, legacy: Option<String>) -> Result<String, String> {
    let non_empty = |value: Option<String>| -> Option<String> {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let query = non_empty(query);
    let legacy = non_empty(legacy);

    let query =
        query.ok_or_else(|| format!("missing required environment variable {QUERY_BUCKET_ENV}"))?;

    if let Some(legacy) = legacy {
        if legacy != query {
            return Err(format!(
                "{LEGACY_BUCKET_ENV} ({legacy}) differs from {QUERY_BUCKET_ENV} ({query}); \
                 {LEGACY_BUCKET_ENV} is deprecated — unset it and publish to {QUERY_BUCKET_ENV}"
            ));
        }
    }

    Ok(query)
}

async fn run(args: CliArgs) -> Result<String, String> {
    let bucket = resolve_bucket(
        env::var(QUERY_BUCKET_ENV).ok(),
        env::var(LEGACY_BUCKET_ENV).ok(),
    )?;
    let release_dir = Path::new(&args.release_dir);

    // 1) Verify: 8-step self-consistency + optional model_id/dim expectations.
    let manifest = verify_release_dir(
        release_dir,
        args.expect_model_id.as_deref(),
        args.expect_dim,
    )
    .map_err(|err| err.to_string())?;
    let release_id = manifest.release_id.clone();

    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let storage = AwsPublishStorage::new(bucket, s3_client_from_env(&sdk_config));

    // 2) Resumable per-object install, manifest last.
    upload_release(&storage, release_dir, &manifest).await?;

    // 3) CAS-flip the pointer.
    let result = activate_static_pointer(&storage, &release_id, current_time_millis())
        .await
        .map_err(|err| err.to_string())?;

    let previous = result.previous_release_id.as_deref().unwrap_or("<none>");
    Ok(format!(
        "activated static release {} (previous {previous})",
        result.release_id
    ))
}

/// Installs a verified release under `static/releases/<release_id>/`,
/// resumably, with the manifest last as the completeness marker.
///
/// The `.bin` objects of the manifest's `turbo_version` go first, each
/// CreateOnly; a conflict on an individual object means a prior run already
/// placed it, so we skip it and resume. Only once every `.bin` is present do we
/// upload `release_manifest.json` (also CreateOnly). Because the manifest is
/// written strictly last, its presence in S3 proves the whole release landed —
/// a run that died mid-upload leaves the `.bin` set incomplete but never the
/// manifest, so the next run backfills instead of flipping the pointer at an
/// incomplete release.
async fn upload_release<S: PublishStorage>(
    storage: &S,
    release_dir: &Path,
    manifest: &ReleaseManifest,
) -> Result<(), String> {
    let release_id = &manifest.release_id;
    let output_files = release_output_files(manifest.turbo_version).ok_or_else(|| {
        format!(
            "no static release file set for turbo_version {}",
            manifest.turbo_version
        )
    })?;

    let dir_key = static_release_dir_key(release_id);
    for name in output_files {
        let object_key = format!("{dir_key}/{name}");
        let source = release_dir.join(name);
        match storage
            .upload_file(&object_key, &source, UploadMode::CreateOnly)
            .await
        {
            Ok(()) => {}
            // Already present from a prior (possibly interrupted) run — but
            // "present" is not "correct". `verify_release_dir` only proved the
            // LOCAL dir; a pre-planted object with WRONG bytes under this prefix
            // would otherwise be skipped, the rest uploaded, and the pointer
            // flipped at a corrupt release. So read the remote object back and
            // verify its sha256 + size against the verified manifest entry for
            // this file before treating the conflict as a resumable skip.
            Err(error) if is_create_only_conflict(&error) => {
                verify_remote_object(storage, &object_key, name, manifest).await?;
            }
            Err(error) => {
                return Err(format!(
                    "failed to upload static release object {object_key}: {error}"
                ));
            }
        }
    }

    // Manifest LAST: it is the completeness marker for this release.
    let manifest_key = static_release_manifest_key(release_id);
    let manifest_source = release_dir.join(RELEASE_MANIFEST_FILE);
    let local_manifest = std::fs::read(&manifest_source).map_err(|error| {
        format!(
            "failed to read local manifest {}: {error}",
            manifest_source.display()
        )
    })?;
    match storage
        .upload_file(&manifest_key, &manifest_source, UploadMode::CreateOnly)
        .await
    {
        Ok(()) => {}
        Err(error) if is_create_only_conflict(&error) => {
            // Manifest already present ⇒ a prior run completed the install. Read
            // it back and byte-compare: equal means genuinely already installed
            // (idempotent success); different means two distinct releases share
            // one content-addressed id, which is impossible barring corruption.
            let stored = storage
                .read(&manifest_key)
                .await
                .map_err(|error| {
                    format!("failed to read stored manifest {manifest_key}: {error}")
                })?
                .ok_or_else(|| {
                    format!(
                        "manifest {manifest_key} reported present then vanished during idempotency check"
                    )
                })?;
            if stored.bytes != local_manifest {
                return Err(format!(
                    "static release {release_id} already installed with a DIFFERENT manifest at {manifest_key}: content-addressed ids make this impossible; treating as corruption"
                ));
            }
            eprintln!(
                "static release {release_id} already fully installed; reusing existing objects"
            );
        }
        Err(error) => {
            return Err(format!(
                "failed to upload static release manifest {manifest_key}: {error}"
            ));
        }
    }

    Ok(())
}

/// Verifies a pre-existing remote object against the already-verified local
/// manifest before a per-object CreateOnly conflict is treated as a resumable
/// skip. Reads the object back and compares its byte length and `sha256_hex`
/// against the manifest's `outputs[]` entry for `name`.
///
/// A mismatch means the object under this content-addressed prefix carries
/// different bytes than this release expects (a pre-planted / corrupt object or
/// an impossible id collision) — fatal, so the caller aborts before uploading
/// the manifest completeness marker or flipping the pointer. A vanished object
/// (`None`) is fatal too.
async fn verify_remote_object<S: PublishStorage>(
    storage: &S,
    object_key: &str,
    name: &str,
    manifest: &ReleaseManifest,
) -> Result<(), String> {
    let expected = manifest
        .outputs
        .iter()
        .find(|output| output.name == name)
        .ok_or_else(|| {
            format!(
                "manifest has no output entry for {name}; cannot verify pre-existing {object_key}"
            )
        })?;
    let stored = storage
        .read(object_key)
        .await
        .map_err(|error| format!("failed to read pre-existing object {object_key}: {error}"))?
        .ok_or_else(|| {
            format!(
                "pre-existing object {object_key} reported present then vanished during integrity check"
            )
        })?;

    let actual_size = stored.bytes.len() as u64;
    if actual_size != expected.size_bytes {
        return Err(format!(
            "pre-existing object {object_key} size mismatch: manifest {}, remote {actual_size}; refusing to activate a corrupt release",
            expected.size_bytes
        ));
    }
    let actual_sha = sha256_hex(&stored.bytes);
    if actual_sha != expected.sha256 {
        return Err(format!(
            "pre-existing object {object_key} sha256 mismatch: manifest {}, remote {actual_sha}; refusing to activate a corrupt release",
            expected.sha256
        ));
    }
    Ok(())
}

/// True for the CreateOnly precondition failure `AwsPublishStorage` raises when
/// an object already exists. `PublishError` collapses that case into
/// `Operation { message }`, so we key off the adapter's fixed wording — the only
/// bin-side discriminator the current error shape offers.
fn is_create_only_conflict(error: &PublishError) -> bool {
    matches!(
        error,
        PublishError::Operation { message }
            if message.contains(CREATE_ONLY_CONFLICT_PHRASE)
    )
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(env::args()).map_err(std::io::Error::other)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let summary = runtime.block_on(run(args)).map_err(std::io::Error::other)?;
    println!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ltsearch::error::PublishError;
    use ltsearch::index::{
        CodecMetadata, EmbeddingProfile, InputFingerprint, ManifestCodec, ReleaseManifest,
        ReleaseSource, TurboQuantConfig, RELEASE_MANIFEST_FILE, V3_RELEASE_OUTPUT_FILES,
        V4_RELEASE_OUTPUT_FILES,
    };
    use ltsearch::indexing::{PublishStorage, UploadMode, VersionedObject};
    use ltsearch::models::CorpusType;

    use super::{parse_args, resolve_bucket, upload_release, CliArgs};

    /// An empty bucket that records each upload in the order it arrives.
    #[derive(Clone, Default)]
    struct RecordingStorage {
        uploads: Arc<Mutex<Vec<(String, UploadMode)>>>,
    }

    #[async_trait]
    impl PublishStorage for RecordingStorage {
        async fn upload_directory(
            &self,
            _key: &str,
            _source: &Path,
            _mode: UploadMode,
        ) -> Result<(), PublishError> {
            unreachable!("activation uploads a release one file at a time")
        }

        async fn upload_file(
            &self,
            key: &str,
            _source: &Path,
            mode: UploadMode,
        ) -> Result<(), PublishError> {
            self.uploads.lock().unwrap().push((key.to_string(), mode));
            Ok(())
        }

        async fn read(&self, _key: &str) -> Result<Option<VersionedObject>, PublishError> {
            Ok(None)
        }

        async fn compare_and_swap(
            &self,
            _key: &str,
            _expected_etag: Option<&str>,
            _new_value: &[u8],
        ) -> Result<bool, PublishError> {
            unreachable!("the upload step never touches the pointer")
        }
    }

    /// A manifest as `verify_release_dir` would return it. `upload_release`
    /// reads only `release_id` and `turbo_version`.
    fn verified_manifest(turbo_version: u32, codec: ManifestCodec) -> ReleaseManifest {
        ReleaseManifest {
            manifest_schema_version: 1,
            turbo_version,
            release_id: "rel-1".to_string(),
            source: ReleaseSource {
                kind: "lance".to_string(),
                dataset_path: "/data/static.lance".to_string(),
                table_version: 1,
                table_row_count: 0,
                corpus_type: CorpusType::Legal,
            },
            embedding_profile: EmbeddingProfile {
                model_id: "jina-embeddings-v2".to_string(),
                dim: 512,
            },
            input_fingerprint: InputFingerprint {
                doc_count: 0,
                content_digest: String::new(),
            },
            codec,
            outputs: Vec::new(),
        }
    }

    fn v3_codec() -> ManifestCodec {
        ManifestCodec::V3(CodecMetadata {
            dim: 512,
            centroids_per_dim: 8,
            centroids_seed: 7,
            projection_seed: 11,
        })
    }

    fn v4_codec() -> ManifestCodec {
        ManifestCodec::V4(TurboQuantConfig::prod_v1().to_v4_codec_metadata().unwrap())
    }

    #[tokio::test]
    async fn upload_release_sends_the_versions_file_set_then_the_manifest() {
        for (turbo_version, codec, files) in [
            (3, v3_codec(), &V3_RELEASE_OUTPUT_FILES[..]),
            (4, v4_codec(), &V4_RELEASE_OUTPUT_FILES[..]),
        ] {
            let release_dir = tempfile::tempdir().unwrap();
            std::fs::write(release_dir.path().join(RELEASE_MANIFEST_FILE), b"{}").unwrap();
            let storage = RecordingStorage::default();

            upload_release(
                &storage,
                release_dir.path(),
                &verified_manifest(turbo_version, codec),
            )
            .await
            .unwrap();

            let mut expected: Vec<(String, UploadMode)> = files
                .iter()
                .map(|name| {
                    (
                        format!("static/releases/rel-1/{name}"),
                        UploadMode::CreateOnly,
                    )
                })
                .collect();
            expected.push((
                "static/releases/rel-1/release_manifest.json".to_string(),
                UploadMode::CreateOnly,
            ));
            assert_eq!(
                *storage.uploads.lock().unwrap(),
                expected,
                "turbo_version {turbo_version}"
            );
        }
    }

    #[tokio::test]
    async fn upload_release_uploads_nothing_for_a_version_without_a_file_set() {
        let release_dir = tempfile::tempdir().unwrap();
        std::fs::write(release_dir.path().join(RELEASE_MANIFEST_FILE), b"{}").unwrap();
        let storage = RecordingStorage::default();

        let error = upload_release(
            &storage,
            release_dir.path(),
            &verified_manifest(5, v4_codec()),
        )
        .await
        .unwrap_err();

        assert!(
            error.contains("turbo_version 5"),
            "unexpected error: {error}"
        );
        assert!(storage.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn resolve_bucket_uses_query_var_when_legacy_unset() {
        assert_eq!(
            resolve_bucket(Some("query-bucket".to_string()), None).unwrap(),
            "query-bucket"
        );
    }

    #[test]
    fn resolve_bucket_accepts_legacy_equal_to_query() {
        assert_eq!(
            resolve_bucket(
                Some("same-bucket".to_string()),
                Some("same-bucket".to_string())
            )
            .unwrap(),
            "same-bucket"
        );
    }

    #[test]
    fn resolve_bucket_errors_when_query_missing() {
        let error = resolve_bucket(None, Some("legacy-bucket".to_string())).unwrap_err();
        assert!(
            error.contains("LTSEARCH_QUERY_S3_BUCKET"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn resolve_bucket_treats_blank_query_as_missing() {
        let error = resolve_bucket(Some("   ".to_string()), None).unwrap_err();
        assert!(
            error.contains("LTSEARCH_QUERY_S3_BUCKET"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn resolve_bucket_errors_when_legacy_differs_from_query() {
        let error = resolve_bucket(
            Some("query-bucket".to_string()),
            Some("static-bucket".to_string()),
        )
        .unwrap_err();
        // Names BOTH values so a stale runbook is diagnosable.
        assert!(error.contains("query-bucket"), "unexpected error: {error}");
        assert!(error.contains("static-bucket"), "unexpected error: {error}");
        assert!(
            error.contains("LTSEARCH_STATIC_S3_BUCKET")
                && error.contains("LTSEARCH_QUERY_S3_BUCKET"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_args_accepts_release_only() {
        let parsed = parse_args(["static_activate", "--release", "/tmp/rel"]).unwrap();
        assert_eq!(
            parsed,
            CliArgs {
                release_dir: "/tmp/rel".to_string(),
                expect_model_id: None,
                expect_dim: None,
            }
        );
    }

    #[test]
    fn parse_args_accepts_optional_expectations() {
        let parsed = parse_args([
            "static_activate",
            "--release",
            "/tmp/rel",
            "--expect-model-id",
            "jina-embeddings-v2",
            "--expect-dim",
            "512",
        ])
        .unwrap();
        assert_eq!(
            parsed.expect_model_id.as_deref(),
            Some("jina-embeddings-v2")
        );
        assert_eq!(parsed.expect_dim, Some(512));
    }

    #[test]
    fn parse_args_requires_release() {
        let error = parse_args(["static_activate"]).unwrap_err();
        assert!(error.contains("--release"), "unexpected error: {error}");
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let error =
            parse_args(["static_activate", "--release", "/tmp/rel", "--bogus"]).unwrap_err();
        assert!(error.contains("--bogus"), "unexpected error: {error}");
    }

    #[test]
    fn parse_args_rejects_non_numeric_expect_dim() {
        let error = parse_args([
            "static_activate",
            "--release",
            "/tmp/rel",
            "--expect-dim",
            "not-a-number",
        ])
        .unwrap_err();
        assert!(error.contains("--expect-dim"), "unexpected error: {error}");
    }

    #[test]
    fn parse_args_rejects_missing_release_value() {
        let error = parse_args(["static_activate", "--release"]).unwrap_err();
        assert!(
            error.contains("missing value for --release"),
            "unexpected error: {error}"
        );
    }
}
