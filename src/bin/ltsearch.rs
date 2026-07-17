//! 单二进制入口（#124）：`ltsearch <write|build|query|static-build> [args…]`。
//! 一个 OCI 镜像按命令选择角色（Compose 以 `command: ["write"]` 等注入）；
//! 组合根在 `ltsearch::app`，此处只做手写子命令分派（对齐 `turbo_index_builder.rs`
//! 的无 clap 风格）。未知/缺失子命令打印用法并以非零码退出。

use std::process::ExitCode;

const USAGE: &str = "usage: ltsearch <write|build|query|static-build> [args...]\n\
  write         serve POST /write, POST /delete, GET /health\n\
  build         serve POST /build, GET /health + SQLite queue worker\n\
  query         serve POST /query, GET /health\n\
  static-build  one-shot TurboQuant v3 release from a pinned Lance snapshot: --config <json> --output <dir>\n\
\n\
Local roles read LTSEARCH_LOCAL_ROOT (shared volume holding wal/, artifacts, ltsearch.db)\n\
and LTSEARCH_HTTP_PORT (default 8080).";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let subcommand = args.next();
    let rest: Vec<String> = args.collect();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ltsearch: failed to start tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let result: Result<(), Box<dyn std::error::Error>> = match subcommand.as_deref() {
        Some("write") => runtime.block_on(ltsearch::app::run_write()),
        Some("build") => runtime.block_on(ltsearch::app::run_build()),
        Some("query") => runtime.block_on(ltsearch::app::run_query()),
        Some("static-build") => runtime
            .block_on(ltsearch::app::run_static_build(
                rest.iter().map(String::as_str),
            ))
            .map(|summary| println!("{summary}")),
        Some(other) => {
            eprintln!("ltsearch: unknown subcommand '{other}'\n{USAGE}");
            return ExitCode::from(2);
        }
        None => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ltsearch: {error}");
            ExitCode::FAILURE
        }
    }
}
