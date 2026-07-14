use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::embedding::{
    fixed_generator_from_env, required_provider_from_env, EmbeddingGenerator, EmbeddingProvider,
};
#[cfg(feature = "ltembed")]
use crate::embedding::{ltembed_config_from_env, LTEmbedEmbeddingGenerator};
use crate::error::SearchError;
use crate::index::MmapIndex;
use crate::models::{SearchRequest, SearchResponse};
use crate::query::{
    KeywordSearcher, NoopStaticRetriever, QueryRouter, StaticRetriever, TurboQuantSearcher,
    VectorSearcher,
};
use crate::storage::{
    ActiveManifest, LocalManifestStore, ManifestHead, ManifestStore, ManifestStoreError,
};

pub type QueryRequestHandler =
    Box<dyn Fn(SearchRequest) -> Result<SearchResponse, SearchError> + Send + Sync + 'static>;
pub type SharedQueryRequestHandler = Arc<QueryRequestHandler>;

const ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP: &str =
    "active manifest version changed during bootstrap";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryLambdaError {
    pub error_type: String,
    pub message: String,
}

impl From<SearchError> for QueryLambdaError {
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Validation(source) => Self {
                error_type: "validation_error".into(),
                message: source.to_string(),
            },
            SearchError::Execution { message } => Self {
                error_type: "execution_error".into(),
                message: SearchError::Execution { message }.to_string(),
            },
        }
    }
}

pub fn bootstrap_query_handler_from_env() -> Result<QueryRequestHandler, QueryLambdaError> {
    match required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER") {
        Ok(provider) => bootstrap_query_embedding_handler(provider, None),
        Err(error) => Err(bootstrap_error(error.to_string())),
    }
}

pub fn bootstrap_query_handler_for_version_from_env(
    expected_version: u64,
) -> Result<QueryRequestHandler, QueryLambdaError> {
    match required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER") {
        Ok(provider) => bootstrap_query_embedding_handler(provider, Some(expected_version)),
        Err(error) => Err(bootstrap_error(error.to_string())),
    }
}

pub fn load_active_query_version_from_env() -> Result<u64, QueryLambdaError> {
    let artifact_root = env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .map_err(|_| bootstrap_error("missing LTSEARCH_QUERY_ARTIFACT_ROOT"))?;
    let manifest_store = LocalManifestStore::new(&artifact_root);

    manifest_store
        .load_active_version()
        .map_err(|source| bootstrap_error(format!("failed to load active version: {source}")))
}

/// 与 `load_active_query_version_from_env` 相同，但把「`_head` 尚不存在」
/// （空索引 / 新装）与「读取失败」区分开：前者返回 `Ok(None)`，供健康检查
/// 判定为「模型可用、等待首次导入」的健康态；其余读取错误照常返回 `Err`。
pub fn load_active_query_version_from_env_opt() -> Result<Option<u64>, QueryLambdaError> {
    let artifact_root = env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .map_err(|_| bootstrap_error("missing LTSEARCH_QUERY_ARTIFACT_ROOT"))?;
    let manifest_store = LocalManifestStore::new(&artifact_root);

    match manifest_store.load_active_version() {
        Ok(version) => Ok(Some(version)),
        Err(ManifestStoreError::MissingHead { .. }) => Ok(None),
        Err(source) => Err(bootstrap_error(format!(
            "failed to load active version: {source}"
        ))),
    }
}

/// 模型完整性探针：按 `LTSEARCH_QUERY_EMBEDDING_PROVIDER` 构建 embedding
/// 引擎并做一次 `generate` 探测，返回向量维度。与查询 bootstrap 复用同一段
/// provider 选择/引擎构建逻辑，失败信息保留底层 `LTEmbed bootstrap failed: …`
/// 文本，供 HTTP `/health` 以 503 报告模型不可用的细节。
pub fn probe_query_embedding_from_env() -> Result<usize, String> {
    let provider = required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER")
        .map_err(|error| error.to_string())?;
    let (embedding_generator, _, _) = build_query_embedding_generator(provider)?;
    let embedding = embedding_generator
        .generate("healthcheck")
        .map_err(|error| error.to_string())?;
    Ok(embedding.len())
}

pub fn is_retriable_bootstrap_version_change(error: &QueryLambdaError) -> bool {
    error.error_type == "execution_error"
        && error
            .message
            .contains(ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP)
}

fn bootstrap_query_embedding_handler(
    provider: EmbeddingProvider,
    expected_version: Option<u64>,
) -> Result<QueryRequestHandler, QueryLambdaError> {
    let artifact_root = env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .map_err(|_| bootstrap_error("missing LTSEARCH_QUERY_ARTIFACT_ROOT"))?;
    let manifest_store = LocalManifestStore::new(&artifact_root);
    let active_manifest = manifest_store
        .load_active_manifest()
        .map_err(|source| bootstrap_error(format!("failed to load active manifest: {source}")))?;
    if let Some(expected_version) = expected_version {
        if active_manifest.head.version_id != expected_version {
            return Err(bootstrap_error(format!(
                "{ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP}: expected {expected_version}, got {}",
                active_manifest.head.version_id,
            )));
        }
    }
    let (embedding_generator, dim_mismatch_name, dim_mismatch_message) =
        build_query_embedding_generator(provider).map_err(bootstrap_error)?;
    let embedding = embedding_generator
        .generate("ignored")
        .map_err(|error| bootstrap_error(error.to_string()))?;

    if embedding.len() != active_manifest.manifest.embedding_dim {
        return Err(bootstrap_error(format!(
            "{dim_mismatch_name} {dim_mismatch_message} {} does not match manifest embedding_dim {}",
            embedding.len(),
            active_manifest.manifest.embedding_dim,
        )));
    }

    let manifest_store = FixedManifestStore::new(active_manifest.clone());
    let router = QueryRouter::new(
        manifest_store.clone(),
        embedding_generator,
        KeywordSearcher::new(manifest_store.clone(), &artifact_root),
        VectorSearcher::new(manifest_store, &artifact_root),
    );

    let static_retriever: Box<dyn StaticRetriever> = match try_load_static_searcher(&artifact_root)?
    {
        Some(static_searcher) => Box::new(static_searcher),
        None => Box::new(NoopStaticRetriever),
    };
    let router = router.with_static_retriever(static_retriever);

    Ok(Box::new(move |request| router.search(&request)))
}

/// 按 provider 构建 embedding 引擎，并返回其维度不匹配诊断所用的 env 名与
/// 字段描述。查询 bootstrap 与健康探针共用此段：provider=fixed 读固定向量，
/// provider=ltembed 由 ONNX bundle 构建引擎。错误以 `String` 冒泡，由各调用点
/// 决定包装成 `QueryLambdaError`（bootstrap）还是原样返回（health probe）。
#[allow(clippy::type_complexity)]
fn build_query_embedding_generator(
    provider: EmbeddingProvider,
) -> Result<(Box<dyn EmbeddingGenerator>, &'static str, &'static str), String> {
    match provider {
        EmbeddingProvider::Fixed => Ok((
            Box::new(
                fixed_generator_from_env("LTSEARCH_QUERY_FIXED_EMBEDDING", None)
                    .map_err(|error| error.to_string())?,
            ),
            "LTSEARCH_QUERY_FIXED_EMBEDDING",
            "dimension",
        )),
        #[cfg(feature = "ltembed")]
        EmbeddingProvider::LTEmbed => {
            let config = ltembed_config_from_env(
                "LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR",
                "LTSEARCH_QUERY_LTEMBED_MODEL_PATH",
            )
            .map_err(|error| error.to_string())?;
            Ok((
                // Query side embeds user queries — the engine prepends the
                // model's query prefix itself.
                Box::new(
                    LTEmbedEmbeddingGenerator::from_config(
                        &config,
                        ltembed::engine::EmbeddingInputKind::Query,
                    )
                    .map_err(|error| error.to_string())?,
                ),
                "LTSEARCH_QUERY_LTEMBED",
                "embedding dimension",
            ))
        }
    }
}

pub fn handle_search_request<H>(
    handler: H,
    request: SearchRequest,
) -> Result<SearchResponse, QueryLambdaError>
where
    H: FnOnce(SearchRequest) -> Result<SearchResponse, SearchError>,
{
    handler(request).map_err(QueryLambdaError::from)
}

fn bootstrap_error(message: impl Into<String>) -> QueryLambdaError {
    QueryLambdaError {
        error_type: "execution_error".into(),
        message: format!("query lambda bootstrap failed: {}", message.into()),
    }
}

fn try_load_static_searcher(
    artifact_root: &Path,
) -> Result<Option<TurboQuantSearcher>, QueryLambdaError> {
    let static_dir = env::var("LTSEARCH_QUERY_STATIC_DIR")
        .map(PathBuf::from)
        .ok();
    let static_dir = static_dir.as_deref().unwrap_or(artifact_root);
    let static_dir = static_dir.join("static");
    if !static_dir.exists() {
        return Ok(None);
    }

    let index = MmapIndex::load(&static_dir).map_err(|error| {
        bootstrap_error(format!(
            "failed to load TurboQuant static index from {}: {error}",
            static_dir.display()
        ))
    })?;

    Ok(Some(TurboQuantSearcher::new(Box::leak(Box::new(index)))))
}

#[derive(Debug, Clone)]
struct FixedManifestStore {
    active_manifest: ActiveManifest,
}

impl FixedManifestStore {
    fn new(active_manifest: ActiveManifest) -> Self {
        Self { active_manifest }
    }
}

impl ManifestStore for FixedManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError> {
        Ok(self.active_manifest.head.clone())
    }

    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError> {
        Ok(self.active_manifest.clone())
    }
}
