//! 单二进制 `ltsearch` 的本地（AWS-free）组合根（#124）。
//!
//! 每个入口都从 [`LocalConfig`]（单一 `LTSEARCH_LOCAL_ROOT`）出发，用 SQLite
//! durability（`SqliteDb` 承载耐久事件 / 构建队列 / 活跃指针）+ 文件系统制品
//! （`LocalPublishStorage` 混合存储）组装对应角色的 router 并 `serve`。三个角色
//! 共享同一卷上的同一个 `ltsearch.db`。
//!
//! 不变式：write 角色的 WAL 与队列必须构造自**同一个** `SqliteDb`——`SqliteBuildQueue::
//! append_and_enqueue`（AC-1 原子写路径）在 API 边界校验这一点，错配会直接报错。
//!
//! 本模块不引用任何 `crate::adapters::*`（那是 `#[cfg(feature = "aws")]` 的），
//! `--features local` 构建不携带任何 AWS 客户端。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::FutureExt;

use crate::bootstrap::{
    build_embedding_generator_from_env, build_embedding_provider_from_env,
    probe_build_embedding_from_env, LocalConfig,
};
use crate::build_worker::{run_build_job_loop, ListWalKeysFn};
use crate::error::IndexError;
use crate::http::build::{build_router, BuildServerState, SnapshotBuildRequest};
use crate::http::query::{query_router, QueryServerState};
use crate::http::write::{write_router, WriteServerState};
use crate::http::{port_from_env, serve};
use crate::index::{parse_static_source_lines, StaticIndexBuilder, TurboBuildConfig};
use crate::indexing::{BuildIndexRequest, IndexPublisher, LocalIndexBuilder, PublishRequest};
use crate::local::{
    LocalPublishStorage, SqliteBuildJobSource, SqliteBuildQueue, SqliteDb, SqliteWalStorage,
};
use crate::query_service::QueryService;
use crate::write::{WriteAheadLog, WriteApi};

type AppError = Box<dyn std::error::Error>;

/// 打开（必要时创建）本地根与控制面数据库。所有子命令的公共起点。
fn open_local(config: &LocalConfig) -> Result<SqliteDb, AppError> {
    std::fs::create_dir_all(&config.root)
        .map_err(|error| format!("failed to create local root {:?}: {error}", config.root))?;
    Ok(SqliteDb::open(config.db_path())?)
}

/// write 角色：`POST /write` / `POST /delete` / `GET /health`。
/// WAL 与队列共用一个 `SqliteDb`，写路径经单事务原子落库后才 ack。
pub async fn run_write() -> Result<(), AppError> {
    let config = LocalConfig::from_env()?;
    let db = open_local(&config)?;

    let wal = SqliteWalStorage::new(db.clone());
    let queue = SqliteBuildQueue::new(db);
    let write_api = Arc::new(WriteApi::new(WriteAheadLog::new(wal), queue));

    let ingest_api = write_api.clone();
    let delete_api = write_api.clone();
    let state = WriteServerState {
        ingest: Arc::new(move |documents| {
            let api = ingest_api.clone();
            async move { api.ingest(documents).await }.boxed()
        }),
        delete: Arc::new(move |doc_ids| {
            let api = delete_api.clone();
            async move { api.delete(doc_ids).await }.boxed()
        }),
    };

    let port = port_from_env();
    eprintln!(
        "ltsearch write listening on 0.0.0.0:{port} (root {:?})",
        config.root
    );
    serve(write_router(state), port).await?;
    Ok(())
}

/// build 角色：`POST /build` / `GET /health`，并后台轮询 SQLite 构建队列
/// （claim/lease/retry/dead-letter 语义在队列侧，worker 成功 ack、失败 nack）。
pub async fn run_build() -> Result<(), AppError> {
    let config = LocalConfig::from_env()?;
    let db = open_local(&config)?;

    let state = BuildServerState {
        build: local_build_closure(config.clone(), db.clone()),
        publish: local_publish_closure(config.clone(), db.clone()),
        embedding_probe: Arc::new(build_embedding_probe()),
    };

    let publish_storage = LocalPublishStorage::new(db.clone(), &config.root);
    let worker_state = state.clone();
    eprintln!(
        "ltsearch build: SQLite queue worker enabled on {:?}",
        config.db_path()
    );
    tokio::spawn(run_build_job_loop(
        SqliteBuildJobSource::new(db.clone()),
        worker_state,
        publish_storage,
        local_list_wal_keys(db),
    ));

    let port = port_from_env();
    eprintln!(
        "ltsearch build listening on 0.0.0.0:{port} (root {:?})",
        config.root
    );
    serve(build_router(state), port).await?;
    Ok(())
}

/// query 角色：`POST /query` / `GET /health`。制品与活跃指针都在共享卷上：
/// 未显式设置时把 `LTSEARCH_QUERY_ARTIFACT_ROOT` 默认为本地根，query 侧的
/// manifest 读取会因 `<root>/ltsearch.db` 的存在自动走 SQLite 活跃指针。
pub async fn run_query() -> Result<(), AppError> {
    let config = LocalConfig::from_env()?;
    open_local(&config)?;
    if std::env::var("LTSEARCH_QUERY_ARTIFACT_ROOT").is_err() {
        std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &config.root);
    }

    let state = QueryServerState {
        service: Arc::new(QueryService::new()),
        embedding_probe: Arc::new(query_embedding_probe()),
    };
    let port = port_from_env();
    eprintln!(
        "ltsearch query listening on 0.0.0.0:{port} (root {:?})",
        config.root
    );
    serve(query_router(state), port).await?;
    Ok(())
}

/// static-build 角色：一次性 CLI（非服务）。与 `turbo_index_builder` 同形的
/// `--config <json> --output <dir>`，但静态源从**本地文件路径**（`sources[].key`）
/// 读取 JSONL，而非 S3——本地 profile 不携带 AWS 客户端。
pub async fn run_static_build<I, S>(args: I) -> Result<String, AppError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let parsed = parse_static_build_args(args)?;
    let config_text = std::fs::read_to_string(&parsed.config_path)
        .map_err(|error| format!("failed to read {}: {error}", parsed.config_path))?;
    let config: TurboBuildConfig = serde_json::from_str(&config_text)
        .map_err(|error| format!("failed to parse {}: {error}", parsed.config_path))?;

    let mut chunks = Vec::new();
    for source in &config.sources {
        let bytes = std::fs::read(&source.key)
            .map_err(|error| format!("failed to read static source {}: {error}", source.key))?;
        let mut source_chunks = parse_static_source_lines(&bytes, &source.corpus_type, &source.key)
            .map_err(|error| error.to_string())?;
        chunks.append(&mut source_chunks);
    }

    let provider = build_embedding_provider_from_env().map_err(|error| error.to_string())?;
    let generator =
        build_embedding_generator_from_env(provider).map_err(|error| error.to_string())?;

    let embeddings = vec![None; chunks.len()];
    let result = StaticIndexBuilder::<()>::new()
        .build(
            std::path::Path::new(&parsed.output_dir),
            &chunks,
            &embeddings,
            &generator,
        )
        .map_err(|error| error.to_string())?;

    Ok(format!(
        "built {} static records (dim={}) into {}",
        result.record_count, result.embedding_dim, parsed.output_dir
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaticBuildArgs {
    config_path: String,
    output_dir: String,
}

/// 手写 `--config/--output` 解析（对齐 `turbo_index_builder.rs` 的风格，不引 clap）。
fn parse_static_build_args<I, S>(args: I) -> Result<StaticBuildArgs, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut config_path = None;
    let mut output_dir = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --config".to_string())?;
                config_path = Some(value.as_ref().to_string());
            }
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --output".to_string())?;
                output_dir = Some(value.as_ref().to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(StaticBuildArgs {
        config_path: config_path.ok_or_else(|| "missing required --config".to_string())?,
        output_dir: output_dir.ok_or_else(|| "missing required --output".to_string())?,
    })
}

/// build 闭包：按序读取全部 `wal_keys`（SQLite WAL）→ 构建 embedding 引擎 →
/// `LocalIndexBuilder`（spawn_blocking）。与 `index_builder_server.rs` 的 AWS 版
/// 同构，仅把 WAL 后端从 S3 换成 SQLite、制品根换成本地根。
fn local_build_closure(config: LocalConfig, db: SqliteDb) -> crate::http::build::BuildFn {
    Arc::new(move |request: SnapshotBuildRequest| {
        let config = config.clone();
        let db = db.clone();
        async move {
            let wal = WriteAheadLog::new(SqliteWalStorage::new(db));
            let mut records = Vec::new();
            for wal_key in &request.wal_keys {
                let segment = wal
                    .read(wal_key)
                    .await
                    .map_err(|error| IndexError::Operation {
                        message: format!("failed to read WAL records from {wal_key}: {error}"),
                    })?;
                records.extend(segment);
            }

            let provider =
                build_embedding_provider_from_env().map_err(|error| IndexError::Operation {
                    message: error.to_string(),
                })?;
            let embedding_generator =
                build_embedding_generator_from_env(provider).map_err(|error| {
                    IndexError::Operation {
                        message: error.to_string(),
                    }
                })?;

            // The build is sync + CPU-heavy, so run it off the async runtime.
            // 暂存根与发布目的地分离（见 LocalConfig::staging_dir）。
            let builder = LocalIndexBuilder::new(config.staging_dir(), embedding_generator);
            let build_request = BuildIndexRequest {
                version_id: request.version_id,
                created_at: current_time_millis(),
                embedding_dim: request.embedding_dim,
                records,
            };
            tokio::task::spawn_blocking(move || builder.build(&build_request))
                .await
                .map_err(|error| IndexError::Operation {
                    message: format!("build task panicked: {error}"),
                })?
        }
        .boxed()
    })
}

/// publish 闭包：混合 `LocalPublishStorage`（head→SQLite，制品字节→文件系统）→
/// `IndexPublisher.publish`；`expected` 由调用方注入（HTTP 侧 None，worker 侧
/// 观测到的 head）。
fn local_publish_closure(config: LocalConfig, db: SqliteDb) -> crate::http::build::PublishFn {
    Arc::new(move |manifest, expected: Option<u64>| {
        let config = config.clone();
        let db = db.clone();
        async move {
            let publish_storage = LocalPublishStorage::new(db, &config.root);
            // 发布方从暂存根读制品、写入 root（head 落 SQLite，字节落文件系统）。
            let publisher = IndexPublisher::new(config.staging_dir(), publish_storage);
            publisher
                .publish(&PublishRequest {
                    manifest,
                    expected_current_version: expected,
                    updated_at: current_time_millis(),
                })
                .await
        }
        .boxed()
    })
}

/// WAL 段列举闭包：`SELECT segment_key FROM wal_segments`，供 worker 在每次构建
/// 前取得完整快照输入（对齐 AWS 侧 ListObjectsV2 语义）。
fn local_list_wal_keys(db: SqliteDb) -> ListWalKeysFn {
    Arc::new(move || {
        let wal = SqliteWalStorage::new(db.clone());
        async move { wal.list_wal_keys().await }.boxed()
    })
}

/// 启动时构建一次 probe 闭包；probe 本身按调用惰性初始化 embedding 引擎，
/// 避免模型损坏导致进程退出——健康检查需以 503 报告细节。
fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
    use std::sync::OnceLock;
    static PROBE_RESULT: OnceLock<Result<usize, String>> = OnceLock::new();
    move || {
        PROBE_RESULT
            .get_or_init(probe_build_embedding_from_env)
            .clone()
    }
}

fn query_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
    use std::sync::OnceLock;
    static PROBE_RESULT: OnceLock<Result<usize, String>> = OnceLock::new();
    move || {
        PROBE_RESULT
            .get_or_init(crate::query_lambda::probe_query_embedding_from_env)
            .clone()
    }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_static_build_args_accepts_config_and_output() {
        let parsed =
            parse_static_build_args(["--config", "/tmp/config.json", "--output", "/tmp/out"])
                .unwrap();
        assert_eq!(parsed.config_path, "/tmp/config.json");
        assert_eq!(parsed.output_dir, "/tmp/out");
    }

    #[test]
    fn parse_static_build_args_rejects_unknown_flag() {
        let error = parse_static_build_args(["--bogus"]).unwrap_err();
        assert!(error.contains("unknown argument"));
    }

    #[test]
    fn parse_static_build_args_requires_both_flags() {
        assert!(parse_static_build_args(["--config", "c"])
            .unwrap_err()
            .contains("--output"));
        assert!(parse_static_build_args(["--output", "o"])
            .unwrap_err()
            .contains("--config"));
    }
}
