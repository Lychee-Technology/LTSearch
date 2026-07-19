use std::sync::Mutex;

use crate::query_lambda::{
    bootstrap_query_handler_for_key_from_env, is_retriable_bootstrap_version_change,
    load_active_query_key_from_env, QueryLambdaError, SharedQueryRequestHandler,
};

/// 缓存键类型：`(dynamic_version, static_release_id)`。查询按这一对解析与复用
/// handler，任一半变化都触发重建，单个请求绝不混用两个不同的静态 release。
type QueryCacheKey = (u64, Option<String>);

/// 版本化 handler 缓存条目，泛型于键 `K`（`QueryService` 固化为
/// [`QueryCacheKey`]；`tests/query_service_test.rs` 的护栏用例以 `K = u64` 复用）。
#[derive(Clone)]
pub struct CachedQueryHandler<K> {
    key: K,
    handler: SharedQueryRequestHandler,
}

/// 版本化 handler 缓存服务：封装 S3 制品同步与按 `(version, release_id)` 对复用
/// handler 的逻辑，供 Lambda bin 与 HTTP 查询服务共用。
pub struct QueryService {
    cache: Mutex<Option<CachedQueryHandler<QueryCacheKey>>>,
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
        resolve_versioned_handler_with_retry(&self.cache, load_active_query_key_from_env, |key| {
            bootstrap_query_handler_for_key_from_env(key).map(SharedQueryRequestHandler::new)
        })
    }

    pub fn cached_version(&self) -> Option<u64> {
        self.cache
            .lock()
            .expect("query handler cache lock poisoned")
            .as_ref()
            .map(|cached| cached.key.0)
    }

    /// 当前缓存条目所固定的静态 release id（键的第二半）。`/health` 在 handler
    /// 解析成功后读取本值上报，与刚写入缓存的 pair 一致——无 TOCTOU 窗口。
    pub fn cached_static_release_id(&self) -> Option<String> {
        self.cache
            .lock()
            .expect("query handler cache lock poisoned")
            .as_ref()
            .and_then(|cached| cached.key.1.clone())
    }
}

impl Default for QueryService {
    fn default() -> Self {
        Self::new()
    }
}

// `pub` 而非 `pub(crate)`：搬迁后的缓存行为集成测试位于 `tests/`，需从 crate 外直接驱动这两个泛型函数。
// 泛型于键 `K`：`QueryService` 以 `K = (u64, Option<String>)` 调用；护栏测试以
// `K = u64` 调用（不改仍绿）。键相等即命中缓存，任一半变化即重建。
pub fn resolve_versioned_handler<K, V, B>(
    cache: &Mutex<Option<CachedQueryHandler<K>>>,
    load_key: V,
    bootstrap: B,
) -> Result<SharedQueryRequestHandler, QueryLambdaError>
where
    K: PartialEq + Clone,
    V: FnOnce() -> Result<K, QueryLambdaError>,
    B: FnOnce(K) -> Result<SharedQueryRequestHandler, QueryLambdaError>,
{
    let key = load_key()?;
    let mut state = cache.lock().expect("query handler cache lock poisoned");

    if let Some(cached) = state.as_ref() {
        if cached.key == key {
            return Ok(cached.handler.clone());
        }
    }

    let handler = bootstrap(key.clone())?;
    *state = Some(CachedQueryHandler {
        key,
        handler: handler.clone(),
    });
    Ok(handler)
}

// `pub` 而非 `pub(crate)`：同上，供 `tests/query_service_test.rs` 中搬迁的集成测试直接调用。
pub fn resolve_versioned_handler_with_retry<K, V, B>(
    cache: &Mutex<Option<CachedQueryHandler<K>>>,
    mut load_key: V,
    bootstrap: B,
) -> Result<SharedQueryRequestHandler, QueryLambdaError>
where
    K: PartialEq + Clone,
    V: FnMut() -> Result<K, QueryLambdaError>,
    B: Fn(K) -> Result<SharedQueryRequestHandler, QueryLambdaError>,
{
    match resolve_versioned_handler(cache, &mut load_key, &bootstrap) {
        Ok(handler) => Ok(handler),
        Err(error) if is_retriable_bootstrap_version_change(&error) => {
            resolve_versioned_handler(cache, load_key, &bootstrap)
        }
        Err(error) => Err(error),
    }
}
