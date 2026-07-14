use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::bootstrap::s3_client_from_env;
use crate::query_lambda::{
    bootstrap_query_handler_for_version_from_env, is_retriable_bootstrap_version_change,
    load_active_query_version_from_env, QueryLambdaError, SharedQueryRequestHandler,
};

#[derive(Clone)]
pub struct CachedQueryHandler {
    version_id: u64,
    handler: SharedQueryRequestHandler,
}

/// 版本化 handler 缓存服务：封装 S3 制品同步与按索引版本复用 handler 的逻辑，
/// 供 Lambda bin 与后续 HTTP 查询服务共用。
pub struct QueryService {
    cache: Mutex<Option<CachedQueryHandler>>,
}

impl QueryService {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(None),
        }
    }

    pub async fn sync_artifacts_if_configured(&self) -> Result<(), String> {
        sync_query_artifacts_from_s3_if_configured().await
    }

    pub fn resolve_handler(&self) -> Result<SharedQueryRequestHandler, QueryLambdaError> {
        resolve_versioned_handler_with_retry(
            &self.cache,
            load_active_query_version_from_env,
            |expected_version| {
                bootstrap_query_handler_for_version_from_env(expected_version)
                    .map(SharedQueryRequestHandler::new)
            },
        )
    }

    pub fn cached_version(&self) -> Option<u64> {
        self.cache
            .lock()
            .expect("query handler cache lock poisoned")
            .as_ref()
            .map(|cached| cached.version_id)
    }
}

impl Default for QueryService {
    fn default() -> Self {
        Self::new()
    }
}

// `pub` 而非 `pub(crate)`：搬迁后的缓存行为集成测试位于 `tests/`，需从 crate 外直接驱动这两个泛型函数。
pub fn resolve_versioned_handler<V, B>(
    cache: &Mutex<Option<CachedQueryHandler>>,
    load_active_version: V,
    bootstrap: B,
) -> Result<SharedQueryRequestHandler, QueryLambdaError>
where
    V: FnOnce() -> Result<u64, QueryLambdaError>,
    B: FnOnce(u64) -> Result<SharedQueryRequestHandler, QueryLambdaError>,
{
    let version_id = load_active_version()?;
    let mut state = cache.lock().expect("query handler cache lock poisoned");

    if let Some(cached) = state.as_ref() {
        if cached.version_id == version_id {
            return Ok(cached.handler.clone());
        }
    }

    let handler = bootstrap(version_id)?;
    *state = Some(CachedQueryHandler {
        version_id,
        handler: handler.clone(),
    });
    Ok(handler)
}

// `pub` 而非 `pub(crate)`：同上，供 `tests/query_service_test.rs` 中搬迁的集成测试直接调用。
pub fn resolve_versioned_handler_with_retry<V, B>(
    cache: &Mutex<Option<CachedQueryHandler>>,
    mut load_active_version: V,
    bootstrap: B,
) -> Result<SharedQueryRequestHandler, QueryLambdaError>
where
    V: FnMut() -> Result<u64, QueryLambdaError>,
    B: Fn(u64) -> Result<SharedQueryRequestHandler, QueryLambdaError>,
{
    match resolve_versioned_handler(cache, &mut load_active_version, &bootstrap) {
        Ok(handler) => Ok(handler),
        Err(error) if is_retriable_bootstrap_version_change(&error) => {
            resolve_versioned_handler(cache, load_active_version, &bootstrap)
        }
        Err(error) => Err(error),
    }
}

async fn sync_query_artifacts_from_s3_if_configured() -> Result<(), String> {
    let bucket = match env::var("LTSEARCH_QUERY_S3_BUCKET") {
        Ok(bucket) => bucket,
        Err(_) => return Ok(()),
    };

    let artifact_root = env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
        .map_err(|_| "missing LTSEARCH_QUERY_ARTIFACT_ROOT".to_string())?;
    let artifact_root = PathBuf::from(artifact_root);
    fs::create_dir_all(&artifact_root)
        .map_err(|error| format!("failed to create query artifact root: {error}"))?;

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = s3_client_from_env(&config);

    for prefix in synced_artifact_prefixes() {
        sync_prefix(&client, &bucket, prefix, &artifact_root).await?;
    }

    Ok(())
}

fn synced_artifact_prefixes() -> Vec<&'static str> {
    vec!["index/", "lance/", "static/"]
}

async fn sync_prefix(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    artifact_root: &Path,
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

            let destination = artifact_root.join(key);
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
    fn synced_artifact_prefixes_include_static_artifacts() {
        assert_eq!(
            synced_artifact_prefixes(),
            vec!["index/", "lance/", "static/"]
        );
    }
}
