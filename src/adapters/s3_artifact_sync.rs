//! `ArtifactSync` 的 S3 实现：把活跃版本所需的 index/lance 前缀批量下载到本地
//! `artifact_root`，再按 `static/_head` 指针的 `release_id` 惰性拉取活跃 v3
//! static release（命中本地缓存则跳过）。`#[cfg(feature = "aws")]` 门控，S3 细节
//! 收敛于此，query 侧调用点只依赖中立契约。

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::Client as S3Client;

use crate::bootstrap::s3_client_from_env;
use crate::contracts::ArtifactSync;
use crate::storage::{
    static_release_dir_key, static_release_manifest_key, StaticReleaseHead, STATIC_HEAD_KEY,
};

pub struct S3ArtifactSync {
    bucket: String,
    /// An explicitly-injected client (tests wire a Moto-pointed one here to avoid
    /// racing on process-wide env). When `None`, `sync` builds one from env,
    /// mirroring `AwsPublishStorage::new` vs the env-driven default split.
    client: Option<S3Client>,
}

impl S3ArtifactSync {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            client: None,
        }
    }

    /// Constructs a sync bound to an explicit S3 client, mirroring
    /// [`crate::adapters::s3_publish::AwsPublishStorage::new`]. Used by tests to
    /// inject a Moto client without mutating process env.
    pub fn with_client(bucket: impl Into<String>, client: S3Client) -> Self {
        Self {
            bucket: bucket.into(),
            client: Some(client),
        }
    }
}

#[async_trait]
impl ArtifactSync for S3ArtifactSync {
    async fn sync(&self, artifact_root: &Path) -> Result<(), String> {
        fs::create_dir_all(artifact_root)
            .map_err(|error| format!("failed to create query artifact root: {error}"))?;

        let client = match &self.client {
            Some(client) => client.clone(),
            None => {
                let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                s3_client_from_env(&config)
            }
        };

        // Order is crash-safe (裁定 5): first the dynamic prefixes, then the
        // static pointer + its release, and only last the pointer file itself.
        for prefix in synced_artifact_prefixes() {
            sync_prefix(&client, &self.bucket, prefix, artifact_root).await?;
        }

        sync_static_release(&client, &self.bucket, artifact_root).await?;

        Ok(())
    }
}

fn synced_artifact_prefixes() -> Vec<&'static str> {
    vec!["index/", "lance/"]
}

/// Pulls the active static release, if one exists, then plants the pointer file.
///
/// Steps, in a deliberately crash-safe order: read `static/_head` into memory
/// (absent ⇒ never activated ⇒ nothing to do); parse the `release_id`; pull the
/// full `static/releases/<id>/` directory *only when* the release manifest is not
/// already on local disk (cache hit ⇒ skip the re-pull); and finally write the
/// pointer file with a bare `fs::write`. Writing the pointer *last* means a crash
/// before or during the release pull leaves no local pointer, so the query side
/// safely sees "no static release" rather than a pointer to a half-pulled dir.
///
/// The bare `fs::write` deliberately bypasses `LocalPublishStorage`/`fs_publish`,
/// which reject the `static/_head` key (that pointer lives only in SQLite there).
async fn sync_static_release(
    client: &S3Client,
    bucket: &str,
    artifact_root: &Path,
) -> Result<(), String> {
    let Some(head_bytes) = read_static_head(client, bucket).await? else {
        return Ok(()); // pointer never activated; no static release to serve
    };

    let head = StaticReleaseHead::from_json(&head_bytes)
        .map_err(|error| format!("remote static/_head is malformed: {error}"))?;

    let local_manifest = artifact_root.join(static_release_manifest_key(&head.release_id));
    if !local_manifest.exists() {
        pull_release_via_staging(client, bucket, artifact_root, &head.release_id).await?;
    }

    let head_path = artifact_root.join(STATIC_HEAD_KEY);
    if let Some(parent) = head_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create local static directory: {error}"))?;
    }
    fs::write(&head_path, &head_bytes)
        .map_err(|error| format!("failed to write local static/_head: {error}"))?;

    Ok(())
}

/// Monotonic per-process counter that, together with the process id, makes
/// every staging directory name unique per pull attempt.
static STAGING_ATTEMPT: AtomicU64 = AtomicU64::new(0);

/// Downloads `static/releases/<id>/` into a per-attempt-unique
/// `.<id>-staging-<pid>-<n>` sibling and then atomically renames it into the
/// final location, mirroring the staging semantics of
/// `install_into_managed_store`.
///
/// The staging hop is what makes the lazy `!manifest.exists()` gate in
/// [`sync_static_release`] reliable: S3 lists keys lexicographically, so
/// `release_manifest.json` downloads *before* the `turbo_static_*.bin` files —
/// a crash mid-pull into the final directory would strand a manifest-first
/// partial release that the gate would forever misread as complete. Staging
/// makes the invariant "final dir exists ⇒ release is complete" hold statically:
/// a final dir can only come from renaming a fully-downloaded,
/// manifest-verified staging dir, and no pull can touch another pull's staging.
///
/// - Each attempt stages under its own unique directory and **never** touches
///   any other `.<id>-staging-*` directory: those are either concurrent pulls
///   in flight or lazy garbage from a crashed process. Crash residue is
///   deliberately left in place — a shared deterministic staging path with an
///   entry wipe would let concurrent pulls delete each other's half-downloaded
///   files and promote an incomplete dir. The residue is a hidden dot-dir,
///   bounded by the number of crashes, and Lambda `/tmp` is short-lived anyway.
/// - The staging dir must actually contain the release manifest before it is
///   promoted (an unexpectedly empty remote listing must not mint an empty
///   final dir that would defeat the gate); on failure the attempt removes its
///   own staging dir.
/// - If the rename fails and the final release manifest is already present, a
///   concurrent pull promoted a complete release first: this attempt's staging
///   copy is discarded and the release is treated as installed. A manifest-less
///   final dir (legacy residue) does NOT count as a winner — the rename error
///   surfaces instead of being swallowed.
async fn pull_release_via_staging(
    client: &S3Client,
    bucket: &str,
    artifact_root: &Path,
    release_id: &str,
) -> Result<(), String> {
    let final_dir = artifact_root.join(static_release_dir_key(release_id));
    let releases_parent = final_dir
        .parent()
        .ok_or_else(|| format!("release dir {} has no parent", final_dir.display()))?
        .to_path_buf();
    let attempt = STAGING_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let staging = releases_parent.join(format!(
        ".{release_id}-staging-{}-{attempt}",
        std::process::id()
    ));

    fs::create_dir_all(&staging)
        .map_err(|error| format!("failed to create staging {}: {error}", staging.display()))?;

    let release_prefix = format!("{}/", static_release_dir_key(release_id));
    if let Err(error) = sync_prefix_to(client, bucket, &release_prefix, &|key| {
        // Map `static/releases/<id>/<file...>` under the staging dir. The list
        // is prefix-filtered, so a key outside the prefix is an S3/SDK bug —
        // fail loudly instead of silently writing somewhere unexpected.
        let relative = key.strip_prefix(&release_prefix).ok_or_else(|| {
            format!("listed key {key} does not start with requested prefix {release_prefix}")
        })?;
        Ok(staging.join(relative))
    })
    .await
    {
        // Self-clean the partial staging dir before surfacing the download
        // error, mirroring the manifest-check and rename-race branches below.
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    if !staging.join(crate::index::RELEASE_MANIFEST_FILE).exists() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "remote release {release_id} is incomplete: no release manifest under {release_prefix}"
        ));
    }

    match fs::rename(&staging, &final_dir) {
        Ok(()) => Ok(()),
        Err(_) if final_dir.join(crate::index::RELEASE_MANIFEST_FILE).exists() => {
            // A concurrent pull promoted a complete release first; ours is
            // redundant. Only our own staging dir is removed.
            let _ = fs::remove_dir_all(&staging);
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to promote staged release {} into {}: {error}",
            staging.display(),
            final_dir.display()
        )),
    }
}

/// Reads the `static/_head` object into memory, folding a missing pointer
/// (`NoSuchKey`) into `Ok(None)` — the normal "never activated" steady state —
/// while surfacing every other S3 error as `Err`.
async fn read_static_head(client: &S3Client, bucket: &str) -> Result<Option<Vec<u8>>, String> {
    match client
        .get_object()
        .bucket(bucket)
        .key(STATIC_HEAD_KEY)
        .send()
        .await
    {
        Ok(object) => {
            let bytes = object
                .body
                .collect()
                .await
                .map_err(|error| format!("failed to read static/_head body from S3: {error}"))?
                .into_bytes()
                .to_vec();
            Ok(Some(bytes))
        }
        Err(error) if is_missing_object_error(&error) => Ok(None),
        Err(error) => Err(format!("failed to load static/_head from S3: {error}")),
    }
}

fn is_missing_object_error(
    error: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    matches!(
        error
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("NoSuchKey") | Some("NotFound")
    )
}

async fn sync_prefix(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    artifact_root: &Path,
) -> Result<(), String> {
    sync_prefix_to(client, bucket, prefix, &|key| Ok(artifact_root.join(key))).await
}

/// Downloads every object under `prefix`, writing each to the path returned by
/// `destination_for(key)`. The mirrored-layout batch pull (`sync_prefix`) and
/// the staged release pull (which redirects keys into a staging dir) share this
/// one list+get loop. The mapper is fallible so callers can reject keys that
/// violate their layout assumptions instead of silently redirecting them.
async fn sync_prefix_to(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    destination_for: &(dyn Fn(&str) -> Result<std::path::PathBuf, String> + Sync),
) -> Result<(), String> {
    let mut continuation_token = None;

    loop {
        let mut request = client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(token) = continuation_token.as_deref() {
            request = request.continuation_token(token);
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("failed to list {prefix} objects from S3: {error}"))?;

        for object in response.contents() {
            let Some(key) = object.key() else {
                continue;
            };
            if key.ends_with('/') {
                continue;
            }

            let body = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|error| format!("failed to download {key} from S3: {error}"))?
                .body
                .collect()
                .await
                .map_err(|error| format!("failed to read {key} body from S3: {error}"))?
                .into_bytes();

            let destination = destination_for(key)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("failed to create local artifact directories: {error}")
                })?;
            }
            fs::write(&destination, body)
                .map_err(|error| format!("failed to write local artifact {key}: {error}"))?;
        }

        if !response.is_truncated().unwrap_or(false) {
            break;
        }
        continuation_token = response
            .next_continuation_token()
            .map(|value| value.to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_artifact_prefixes_excludes_static() {
        let prefixes = synced_artifact_prefixes();
        assert_eq!(prefixes, vec!["index/", "lance/"]);
        assert!(
            !prefixes.contains(&"static/"),
            "static/ is pulled lazily by release_id, never as a batch prefix"
        );
    }
}
