use std::sync::Mutex;

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
        #[cfg(feature = "aws")]
        {
            use crate::contracts::ArtifactSync;
            let bucket = match std::env::var("LTSEARCH_QUERY_S3_BUCKET") {
                Ok(bucket) => bucket,
                Err(_) => return Ok(()),
            };
            let artifact_root = std::env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
                .map_err(|_| "missing LTSEARCH_QUERY_ARTIFACT_ROOT".to_string())?;
            crate::adapters::s3_artifact_sync::S3ArtifactSync::new(bucket)
                .sync(std::path::Path::new(&artifact_root))
                .await
        }
        #[cfg(not(feature = "aws"))]
        {
            use crate::contracts::ArtifactSync;
            // local profile: artifacts already on disk (mounted volume / local build)
            crate::local::NoopArtifactSync::new()
                .sync(std::path::Path::new("."))
                .await
        }
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
