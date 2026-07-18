//! `static_activate` — the AWS twin of the local `ltsearch static-activate` CLI.
//!
//! Verifies a built v3 release directory, uploads it immutably to S3 under
//! `static/releases/<release_id>/` (CreateOnly / `If-None-Match`), then
//! compare-and-swaps the `static/_head` pointer to it. The three steps are
//! strictly ordered: verify precedes upload, upload precedes the pointer swap.
//!
//! Idempotent *and* resumable: because `release_id` is content-derived,
//! re-running the same release re-targets the same immutable keys. The nine
//! `.bin` objects are uploaded one at a time — a CreateOnly conflict on any one
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
//! env: LTSEARCH_STATIC_S3_BUCKET (required)
//! ```

use std::env;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::adapters::s3_publish::{AwsPublishStorage, CREATE_ONLY_CONFLICT_PHRASE};
use ltsearch::bootstrap::s3_client_from_env;
use ltsearch::error::PublishError;
use ltsearch::index::{RELEASE_MANIFEST_FILE, V3_RELEASE_OUTPUT_FILES};
use ltsearch::indexing::{
    activate_static_pointer, verify_release_dir, PublishStorage, StaticActivateError, UploadMode,
};
use ltsearch::storage::{static_release_dir_key, static_release_manifest_key};

/// The bucket holding the static release tree and its `static/_head` pointer.
const BUCKET_ENV: &str = "LTSEARCH_STATIC_S3_BUCKET";

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

async fn run(args: CliArgs) -> Result<String, String> {
    let bucket = match env::var(BUCKET_ENV) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            return Err(format!(
                "missing required environment variable {BUCKET_ENV}"
            ))
        }
    };
    let release_dir = Path::new(&args.release_dir);

    // 1) Verify: 8-step self-consistency + optional model_id/dim expectations.
    let manifest = verify_release_dir(
        release_dir,
        args.expect_model_id.as_deref(),
        args.expect_dim,
    )
    .map_err(describe_activate_error)?;
    let release_id = manifest.release_id.clone();

    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let storage = AwsPublishStorage::new(bucket, s3_client_from_env(&sdk_config));

    // 2) Resumable per-object install with manifest-last as the completeness
    //    marker. Upload the nine `.bin` objects first, each CreateOnly; a
    //    conflict on an individual object means a prior run already placed it,
    //    so we skip it and resume. Only once every `.bin` is present do we
    //    upload `release_manifest.json` (also CreateOnly). Because the manifest
    //    is written strictly last, its presence in S3 proves the whole release
    //    landed — a run that died mid-upload leaves the `.bin` set incomplete
    //    but never the manifest, so the next run backfills instead of flipping
    //    the pointer at an incomplete release.
    let dir_key = static_release_dir_key(&release_id);
    for name in V3_RELEASE_OUTPUT_FILES {
        let object_key = format!("{dir_key}/{name}");
        let source = release_dir.join(name);
        match storage
            .upload_file(&object_key, &source, UploadMode::CreateOnly)
            .await
        {
            Ok(()) => {}
            // Already present from a prior (possibly interrupted) run — resume.
            Err(error) if is_create_only_conflict(&error) => {}
            Err(error) => {
                return Err(format!(
                    "failed to upload static release object {object_key}: {error}"
                ));
            }
        }
    }

    // Manifest LAST: it is the completeness marker for this release.
    let manifest_key = static_release_manifest_key(&release_id);
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

    // 3) CAS-flip the pointer.
    let result = activate_static_pointer(&storage, &release_id, current_time_millis())
        .await
        .map_err(describe_activate_error)?;

    let previous = result.previous_release_id.as_deref().unwrap_or("<none>");
    Ok(format!(
        "activated static release {} (previous {previous})",
        result.release_id
    ))
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

/// Renders [`StaticActivateError`] (which only derives `Debug`) as a readable
/// one-line summary, matching `app.rs`'s local-CLI mapping.
fn describe_activate_error(error: StaticActivateError) -> String {
    match error {
        StaticActivateError::Verify { message } => {
            format!("release verification failed: {message}")
        }
        StaticActivateError::LostCas { release_id } => {
            format!("static pointer CAS lost for release {release_id} (concurrent writer won)")
        }
        StaticActivateError::Storage(error) => format!("publish storage error: {error}"),
        StaticActivateError::Io { message } => format!("install failed: {message}"),
    }
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
    use super::{parse_args, CliArgs};

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
