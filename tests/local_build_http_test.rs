//! local build 角色真实链路的 router 级集成测试（#140）：不 stub build/publish，
//! 用 `local_build_role` 接真实 SQLite WAL → fixed embedding → 本地发布链路，
//! 覆盖「write 公开响应的 batch_id + wal_key 直接驱动显式 /build 发布版本 1」、
//! 「缺段经真实 WAL 读取路径映射 build_error」与 worker 开关的组装语义。
#![cfg(feature = "local")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use ltsearch::app::local_build_role;
use ltsearch::bootstrap::LocalConfig;
use ltsearch::http::build::build_router;
use ltsearch::local::{SqliteBuildQueue, SqliteDb, SqliteWalStorage};
use ltsearch::models::Document;
use ltsearch::write::{WriteAheadLog, WriteApi};

/// 各测试设置**相同**的 fixed embedding env（只设不删、值恒定），与
/// docker-compose.local.yml 的本地拓扑一致，避免同二进制并行测试的 env 竞态。
fn set_fixed_embedding_env() {
    std::env::set_var("LTSEARCH_BUILD_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_BUILD_FIXED_EMBEDDING", "0.1,0.2,0.3");
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

fn sample_document(doc_id: &str) -> Document {
    Document {
        doc_id: doc_id.into(),
        text: format!("hello from {doc_id}"),
        embedding: None,
        metadata: Default::default(),
        timestamp: 1_700_000_000_000,
    }
}

async fn post_build(app: axum::Router, payload: serde_json::Value) -> axum::response::Response {
    app.oneshot(
        Request::post("/build")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap(),
    )
    .await
    .unwrap()
}

// 1) #140 显式工作流全链路：真实 write 链路的公开响应（batch_id + wal_key）直接
//    交给真实 local /build 发布版本 1；worker 显式禁用（组装不产生后台 future），
//    HTTP /build 仍完整服务并报告激活版本、前一版本与文档数。
#[tokio::test]
async fn write_response_wal_key_drives_explicit_build_to_version_1() {
    set_fixed_embedding_env();
    let dir = tempfile::tempdir().unwrap();
    let config = LocalConfig {
        root: dir.path().to_path_buf(),
    };
    let db = SqliteDb::open(config.db_path()).unwrap();

    // 真实 write 链路：WAL 与队列构造自同一个 SqliteDb（AC-1 原子写路径）。
    let write_api = WriteApi::new(
        WriteAheadLog::new(SqliteWalStorage::new(db.clone())),
        SqliteBuildQueue::new(db.clone()),
    );
    let write_response = write_api
        .ingest(vec![sample_document("doc-1"), sample_document("doc-2")])
        .await
        .unwrap();
    assert!(!write_response.wal_key.is_empty());

    // worker 显式禁用：组装决策即「不产生后台 future」。
    let (state, worker) = local_build_role(&config, db, false);
    assert!(worker.is_none(), "disabled worker must not be assembled");

    let response = post_build(
        build_router(state),
        json!({
            "batch_id": write_response.batch_id,
            "wal_key": write_response.wal_key,
            "version_id": 1,
            "embedding_dim": 3,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["activated_version_id"], 1);
    assert!(json["previous_version_id"].is_null());
    assert_eq!(json["document_count"], 2);
}

// 2) 缺段经真实 SQLite WAL 读取路径（SqliteWalStorage::read → local_build_closure）
//    映射既定 build_error 信封，而非 stub 伪造的 IndexError。
#[tokio::test]
async fn missing_wal_segment_maps_to_build_error_through_real_read_path() {
    set_fixed_embedding_env();
    let dir = tempfile::tempdir().unwrap();
    let config = LocalConfig {
        root: dir.path().to_path_buf(),
    };
    let db = SqliteDb::open(config.db_path()).unwrap();

    let (state, worker) = local_build_role(&config, db, false);
    assert!(worker.is_none());

    let response = post_build(
        build_router(state),
        json!({
            "batch_id": "batch-missing",
            "wal_key": "wal/2026/07/20/batch-missing.jsonl",
            "version_id": 1,
            "embedding_dim": 3,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "build_error");
    let message = json["message"].as_str().unwrap();
    assert!(message.contains("segment not found"), "message: {message}");
    assert!(
        message.contains("wal/2026/07/20/batch-missing.jsonl"),
        "message: {message}"
    );
}

// 3) 默认（启用）语义的组装侧证据：worker_enabled=true 时组装出后台 future，
//    与 run_build 中 `build_worker_enabled_from_env()` 的默认 true 相衔接。
#[tokio::test]
async fn worker_enabled_assembly_produces_background_worker_future() {
    set_fixed_embedding_env();
    let dir = tempfile::tempdir().unwrap();
    let config = LocalConfig {
        root: dir.path().to_path_buf(),
    };
    let db = SqliteDb::open(config.db_path()).unwrap();

    let (_state, worker) = local_build_role(&config, db, true);
    assert!(worker.is_some(), "default-enabled worker must be assembled");
}
