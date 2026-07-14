use std::sync::Arc;

use ltsearch::http::query::{query_router, QueryServerState};
use ltsearch::http::{port_from_env, serve};
use ltsearch::query_service::QueryService;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let state = QueryServerState {
            service: Arc::new(QueryService::new()),
            embedding_probe: Arc::new(build_embedding_probe()),
        };
        let port = port_from_env();
        eprintln!("ltsearch-query-server listening on 0.0.0.0:{port}");
        serve(query_router(state), port).await?;
        Ok(())
    })
}

/// 启动时构建一次 probe 闭包；probe 本身按调用惰性初始化 embedding 引擎，
/// 避免模型损坏导致进程直接退出——健康检查需要能以 503 报告细节。
fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
    use std::sync::OnceLock;
    static PROBE_RESULT: OnceLock<Result<usize, String>> = OnceLock::new();
    move || {
        PROBE_RESULT
            .get_or_init(ltsearch::query_lambda::probe_query_embedding_from_env)
            .clone()
    }
}
